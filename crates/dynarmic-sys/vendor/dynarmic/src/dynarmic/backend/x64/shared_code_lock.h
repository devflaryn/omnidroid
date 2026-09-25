/* This file is part of the dynarmic project.
 * Copyright (c) 2026 Omnidroid contributors
 * SPDX-License-Identifier: 0BSD
 */

#pragma once

#if defined(_WIN32)
#    include <shared_mutex>
#else
#    include <pthread.h>
#endif

namespace Dynarmic::Backend::X64 {

/// Omnidroid patch 0022: the lock of a shared code cache -- dispatcher lookups shared, emission
/// and invalidation exclusive -- which must not let a stream of lookups starve a writer.
/// `std::shared_mutex` is an SRW lock on Windows, which does not. On glibc it is a
/// `pthread_rwlock_t` of the default kind, which prefers readers: MEASURED on the Linux host,
/// eight threads looking blocks up held off an invalidating thread so that it managed 4,675
/// invalidations in 3 s against Windows' ~750,000. So there it is a writer-preferring rwlock.
class SharedCodeLock {
public:
#if defined(_WIN32)
    void lock() { m.lock(); }
    bool try_lock() { return m.try_lock(); }
    void unlock() { m.unlock(); }
    void lock_shared() { m.lock_shared(); }
    void unlock_shared() { m.unlock_shared(); }

private:
    std::shared_mutex m;
#else
    SharedCodeLock() {
        pthread_rwlockattr_t attr;
        pthread_rwlockattr_init(&attr);
#    if defined(__GLIBC__)
        pthread_rwlockattr_setkind_np(&attr, PTHREAD_RWLOCK_PREFER_WRITER_NONRECURSIVE_NP);
#    endif
        pthread_rwlock_init(&m, &attr);
        pthread_rwlockattr_destroy(&attr);
    }
    ~SharedCodeLock() { pthread_rwlock_destroy(&m); }
    SharedCodeLock(const SharedCodeLock&) = delete;
    SharedCodeLock& operator=(const SharedCodeLock&) = delete;

    void lock() { pthread_rwlock_wrlock(&m); }
    bool try_lock() { return pthread_rwlock_trywrlock(&m) == 0; }
    void unlock() { pthread_rwlock_unlock(&m); }
    void lock_shared() { pthread_rwlock_rdlock(&m); }
    void unlock_shared() { pthread_rwlock_unlock(&m); }

private:
    pthread_rwlock_t m;
#endif
};

}  // namespace Dynarmic::Backend::X64
