/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include "sigbus_memops.h"
#include "sigbus_memops_config.h"

#include <errno.h>
#include <string.h>

bool sigbus_is_protected(void) {
  return SIGBUS_MEMOPS_HAS_PROTECTION;
}

#if SIGBUS_MEMOPS_WINDOWS_SEH

// @lint-ignore SPELL
#include <excpt.h>
#include <windows.h>

int sigbus_install_handler(void) {
  return 0;
}

void sigbus_set_retry_budget(unsigned int retry_budget) {
  (void)retry_budget;
}

static int sigbus_memops_filter(unsigned long code) {
  return code == EXCEPTION_IN_PAGE_ERROR ? EXCEPTION_EXECUTE_HANDLER
                                         : EXCEPTION_CONTINUE_SEARCH;
}

bool sigbus_test_raise_in_page_error(void) {
  __try {
    RaiseException(EXCEPTION_IN_PAGE_ERROR, 0, 0, NULL);
  } __except (sigbus_memops_filter(GetExceptionCode())) {
    return true;
  }
  return false;
}

bool sigbus_try_memcpy(void* dst, const void* src, size_t len) {
  __try {
    if (len != 0) {
      memcpy(dst, src, len);
    }
    return true;
  } __except (sigbus_memops_filter(GetExceptionCode())) {
    return false;
  }
}

bool sigbus_try_read(const void* src, size_t len) {
  __try {
    const volatile unsigned char* bytes = src;
    for (size_t i = 0; i < len; ++i) {
      (void)bytes[i];
    }
    return true;
  } __except (sigbus_memops_filter(GetExceptionCode())) {
    return false;
  }
}

bool sigbus_try_store_u64(void* dst, uint64_t value) {
  __try {
    *(volatile uint64_t*)dst = value;
    return true;
  } __except (sigbus_memops_filter(GetExceptionCode())) {
    return false;
  }
}

#elif SIGBUS_MEMOPS_HAS_PROTECTION

#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <unistd.h>

#if SIGBUS_MEMOPS_DARWIN_AARCH64
#include <mach/arm/thread_status.h>
#include <sys/ucontext.h>
#else
#include <ucontext.h>
#endif

// Defined by labels in the protected functions' inline assembly.
extern void sigbus_try_memcpy_fault_pc(void);
extern void sigbus_try_memcpy_recover_pc(void);
extern void sigbus_try_read_fault_pc(void);
extern void sigbus_try_read_recover_pc(void);
extern void sigbus_try_store_u64_fault_pc(void);
extern void sigbus_try_store_u64_recover_pc(void);
#if SIGBUS_MEMOPS_ARCH_AARCH64
// Only AArch64 splits the destination write into its own protected
// instruction; x86-64 faults on the same `rep movsb` for both accesses.
extern void sigbus_try_memcpy_store_fault_pc(void);
#endif

static struct sigaction sigbus_memops_previous_action;
static pthread_once_t sigbus_memops_install_once = PTHREAD_ONCE_INIT;
static int sigbus_memops_install_error;
// Shared between normal code and the SIGBUS handler.
// NOLINTNEXTLINE(facebook-avoid-non-const-global-variables)
static _Atomic unsigned int sigbus_retry_budget;

_Static_assert(
    ATOMIC_INT_LOCK_FREE == 2,
    "SIGBUS retry budget must be lock-free");

static void
sigbus_memops_signal_handler(int signo, siginfo_t* info, void* ucontext) {
  if (sigbus_try_handle(signo, info, ucontext)) {
    return;
  }

  if (sigbus_memops_previous_action.sa_handler == SIG_IGN ||
      sigbus_memops_previous_action.sa_handler == SIG_DFL) {
    // A synchronous fault re-runs the faulting instruction after this handler
    // returns. Restore the original disposition first so it does not loop back
    // into this handler.
    if (sigaction(signo, &sigbus_memops_previous_action, NULL) != 0) {
      _exit(128 + signo);
    }
    return;
  }

  if (sigbus_memops_previous_action.sa_flags & SA_SIGINFO) {
    sigbus_memops_previous_action.sa_sigaction(signo, info, ucontext);
  } else {
    sigbus_memops_previous_action.sa_handler(signo);
  }
}

static void sigbus_memops_install_handler_once(void) {
  struct sigaction action = {0};
  action.sa_sigaction = sigbus_memops_signal_handler;
  action.sa_flags = SA_SIGINFO | SA_ONSTACK;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGBUS, &action, &sigbus_memops_previous_action) != 0) {
    sigbus_memops_install_error = errno;
  }
}

int sigbus_install_handler(void) {
  int error = pthread_once(
      &sigbus_memops_install_once, sigbus_memops_install_handler_once);
  return error != 0 ? error : sigbus_memops_install_error;
}

void sigbus_set_retry_budget(unsigned int retry_budget) {
  atomic_store_explicit(
      &sigbus_retry_budget, retry_budget, memory_order_relaxed);
}

static bool sigbus_try_consume_retry(void) {
  unsigned int budget =
      atomic_load_explicit(&sigbus_retry_budget, memory_order_relaxed);
  while (budget != 0) {
    if (atomic_compare_exchange_weak_explicit(
            &sigbus_retry_budget,
            &budget,
            budget - 1,
            memory_order_relaxed,
            memory_order_relaxed)) {
      return true;
    }
  }
  return false;
}

bool sigbus_try_handle(int signo, siginfo_t* info, void* opaque) {
  if (signo != SIGBUS || info == NULL || opaque == NULL) {
    return false;
  }
#ifdef BUS_MCEERR_AO
  if (info->si_code == BUS_MCEERR_AO) {
    return false;
  }
#endif

  ucontext_t* context = opaque;

#if SIGBUS_MEMOPS_ARCH_X86_64
  greg_t* registers = context->uc_mcontext.gregs;

  // x86-64 reports source-read and destination-write faults at the same
  // `rep movsb` program counter.
  uintptr_t pc = (uintptr_t)registers[REG_RIP];
  if (pc == (uintptr_t)&sigbus_try_memcpy_fault_pc) {
    registers[REG_RIP] = (greg_t)(uintptr_t)&sigbus_try_memcpy_recover_pc;
    return true;
  }

  if (pc == (uintptr_t)&sigbus_try_read_fault_pc) {
    registers[REG_RIP] = (greg_t)(uintptr_t)&sigbus_try_read_recover_pc;
    return true;
  }

  if (pc == (uintptr_t)&sigbus_try_store_u64_fault_pc) {
    registers[REG_RIP] = (greg_t)(uintptr_t)&sigbus_try_store_u64_recover_pc;
    return true;
  }
#elif SIGBUS_MEMOPS_ARCH_AARCH64
#if SIGBUS_MEMOPS_DARWIN_AARCH64
  arm_thread_state64_t* registers = &context->uc_mcontext->__ss;
  void (*pc)(void) = arm_thread_state64_get_pc_fptr(*registers);
  if (pc == &sigbus_try_memcpy_fault_pc ||
      pc == &sigbus_try_memcpy_store_fault_pc) {
    arm_thread_state64_set_pc_fptr(*registers, &sigbus_try_memcpy_recover_pc);
    return true;
  }

  if (pc == &sigbus_try_read_fault_pc) {
    arm_thread_state64_set_pc_fptr(*registers, &sigbus_try_read_recover_pc);
    return true;
  }

  if (pc == &sigbus_try_store_u64_fault_pc) {
    arm_thread_state64_set_pc_fptr(
        *registers, &sigbus_try_store_u64_recover_pc);
    return true;
  }
#else
  uintptr_t pc = (uintptr_t)context->uc_mcontext.pc;
  if (pc == (uintptr_t)&sigbus_try_memcpy_fault_pc ||
      pc == (uintptr_t)&sigbus_try_memcpy_store_fault_pc) {
    context->uc_mcontext.pc = (uintptr_t)&sigbus_try_memcpy_recover_pc;
    return true;
  }

  if (pc == (uintptr_t)&sigbus_try_read_fault_pc) {
    context->uc_mcontext.pc = (uintptr_t)&sigbus_try_read_recover_pc;
    return true;
  }

  if (pc == (uintptr_t)&sigbus_try_store_u64_fault_pc) {
    context->uc_mcontext.pc = (uintptr_t)&sigbus_try_store_u64_recover_pc;
    return true;
  }
#endif
#endif

#ifdef BUS_ADRERR
  if (info->si_code == BUS_ADRERR && sigbus_try_consume_retry()) {
    const int saved_errno = errno;
    static const char message[] = "sigbus-memops: retrying unhandled SIGBUS\n";
    (void)write(STDERR_FILENO, message, sizeof(message) - 1);
    errno = saved_errno;
    return true;
  }
#endif

  return false;
}

#else

int sigbus_install_handler(void) {
  return 0;
}

void sigbus_set_retry_budget(unsigned int retry_budget) {
  (void)retry_budget;
}

#ifndef _WIN32
bool sigbus_try_handle(int signo, siginfo_t* info, void* opaque) {
  (void)signo;
  (void)info;
  (void)opaque;
  return false;
}
#endif

bool sigbus_try_memcpy(void* dst, const void* src, size_t len) {
  if (len == 0) {
    return true;
  }
  memcpy(dst, src, len);
  return true;
}

bool sigbus_try_read(const void* src, size_t len) {
  if (len == 0) {
    return true;
  }
  const volatile unsigned char* bytes = src;
  for (size_t i = 0; i < len; ++i) {
    (void)bytes[i];
  }
  return true;
}

bool sigbus_try_store_u64(void* dst, uint64_t value) {
  *(volatile uint64_t*)dst = value;
  return true;
}

#endif
