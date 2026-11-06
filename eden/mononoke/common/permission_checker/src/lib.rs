/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod checker;
pub mod dummy;
#[cfg(fbcode_build)]
mod facebook;
mod identity;
mod internal;
mod membership;
#[cfg(not(fbcode_build))]
mod oss;
mod provider;

pub use checker::ArcPermissionChecker;
pub use checker::BoxPermissionChecker;
pub use checker::DenialReason;
pub use checker::PermissionCheckResult;
pub use checker::PermissionChecker;
pub use checker::PermissionCheckerBuilder;
pub use checker::PermissionDenial;
#[cfg(fbcode_build)]
pub use facebook::AccessCheckerProvider;
pub use identity::ClientCategory;
pub use identity::MononokeIdentity;
pub use identity::MononokeIdentitySet;
pub use identity::MononokeIdentitySetExt;
pub use identity::merge_identity_sets;
pub use internal::Acl;
pub use internal::Acls;
pub use internal::InternalAclProvider;
pub use membership::AlwaysMember;
pub use membership::ArcMembershipChecker;
pub use membership::BoxMembershipChecker;
pub use membership::MemberAllowlist;
pub use membership::MembershipChecker;
pub use membership::NeverMember;
pub use provider::AclProvider;

#[cfg(not(fbcode_build))]
pub type DefaultAclProvider = dummy::DummyAclProvider;
