# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# peer.py - repository base classes for mercurial
#
# Copyright 2005, 2006 Olivia Mackall <olivia@selenic.com>
# Copyright 2006 Vadim Gelfer <vadim.gelfer@gmail.com>
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.


from . import error


# abstract batching support


class future:
    """placeholder for a value to be set later"""

    def set(self, value):
        if hasattr(self, "value"):
            raise error.RepoError("future is already set")
        self.value = value


class batcher:
    """base class for batches of commands submittable in a single request

    All methods invoked on instances of this class are simply queued and
    return a future for the result. Once you call submit(), all the queued
    calls are performed and the results set in their respective futures.
    """

    def __init__(self):
        self.calls = []

    def __getattr__(self, name):
        def call(*args, **opts):
            resref = future()
            # Please don't invent non-ascii method names, or you will
            # give core hg a very sad time.
            self.calls.append((name, args, opts, resref))
            return resref

        return call

    def submit(self):
        raise NotImplementedError()


class iterbatcher(batcher):
    def submit(self):
        raise NotImplementedError()

    def results(self):
        raise NotImplementedError()


class localiterbatcher(iterbatcher):
    def __init__(self, local):
        super(iterbatcher, self).__init__()
        self.local = local

    def submit(self):
        # submit for a local iter batcher is a noop
        pass

    def results(self):
        for name, args, opts, resref in self.calls:
            resref.set(getattr(self.local, name)(*args, **opts))
            yield resref.value


def batchable(f):
    """annotation for batchable methods

    Such methods must implement a coroutine as follows:

    @batchable
    def sample(self, one, two=None):
        # Build list of encoded arguments suitable for your wire protocol:
        encargs = [('one', encode(one),), ('two', encode(two),)]
        # Create future for injection of encoded result:
        encresref = future()
        # Return encoded arguments and future:
        yield encargs, encresref
        # Assuming the future to be filled with the result from the batched
        # request now. Decode it:
        yield decode(encresref.value)

    The decorator returns a function which wraps this coroutine as a plain
    method, but adds the original method as an attribute called "batchable",
    which is used by remotebatch to split the call into separate encoding and
    decoding phases.
    """

    def plain(*args, **opts):
        batchable = f(*args, **opts)
        encargsorres, encresref = next(batchable)
        if not encresref:
            return encargsorres  # a local result in this case
        self = args[0]
        cmd = f.__name__  # ensure cmd is ascii bytestr
        encresref.set(self._submitone(cmd, encargsorres))
        return next(batchable)

    setattr(plain, "batchable", f)
    return plain
