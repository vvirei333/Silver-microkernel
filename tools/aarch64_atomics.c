/* Out-of-line AArch64 atomic helpers that GCC emits for libffi compiled
 * without LSE. The real implementations live in libgcc's lse.S with runtime
 * LSE detection; these versions always use LL/SC loops, which are correct on
 * every ARMv8 CPU (QEMU's cortex-a72 included).
 *
 * ABI (libgcc/config/aarch64/lse.S), _sync = acq_rel semantics:
 *   TYPE __aarch64_casN_sync(TYPE new, TYPE expected, TYPE *ptr) -> old
 *   TYPE __aarch64_swpN_sync(TYPE val, TYPE *ptr)                -> old
 *   TYPE __aarch64_ldaddN_sync(TYPE val, TYPE *ptr)              -> old
 *   TYPE __aarch64_ldclrN_sync(TYPE mask, TYPE *ptr)             -> old
 *   TYPE __aarch64_ldsetN_sync(TYPE mask, TYPE *ptr)             -> old
 *   TYPE __aarch64_ldeorN_sync(TYPE mask, TYPE *ptr)             -> old
 * with N in {1,2,4,8} bytes. Generic operand specifiers (%0..) let the
 * compiler pick w- or x-registers by the type's width.
 */
#include <stdint.h>

#define DEFINE_HELPERS(W, T)                                               \
    T __aarch64_cas##W##_sync(T newval, T expected, volatile T *ptr) {     \
        T old, tmp;                                                        \
        uint32_t failed;                                                   \
        for (;;) {                                                         \
            __asm__ volatile("ldaxr %0, %2\n"                              \
                             "cmp %0, %3\n"                                \
                             "b.ne 2f\n"                                   \
                             "stlxr %w1, %4, %2\n"                         \
                             "2:"                                          \
                             : "=&r"(old), "=&r"(failed), "+Q"(*ptr)       \
                             : "r"(expected), "r"(newval)                  \
                             : "memory");                                  \
            if (!failed && old == expected) return old;                    \
            if (old != expected) return old;                               \
        }                                                                  \
    }                                                                      \
    T __aarch64_swp##W##_sync(T val, volatile T *ptr) {                    \
        T old, tmp;                                                        \
        __asm__ volatile("1: ldaxr %0, %2\n"                               \
                         "stlxr %w1, %3, %2\n"                             \
                         "cbnz %w1, 1b"                                    \
                         : "=&r"(old), "=&r"(tmp), "+Q"(*ptr)              \
                         : "r"(val)                                        \
                         : "memory");                                      \
        return old;                                                        \
    }                                                                      \
    T __aarch64_ldadd##W##_sync(T val, volatile T *ptr) {                  \
        T old, newval, tmp;                                                \
        __asm__ volatile("1: ldaxr %0, %3\n"                               \
                         "add %1, %0, %4\n"                                \
                         "stlxr %w2, %1, %3\n"                             \
                         "cbnz %w2, 1b"                                    \
                         : "=&r"(old), "=&r"(newval), "=&r"(tmp), "+Q"(*ptr) \
                         : "r"(val)                                        \
                         : "memory");                                      \
        return old;                                                        \
    }                                                                      \
    T __aarch64_ldclr##W##_sync(T mask, volatile T *ptr) {                 \
        T old, newval, tmp;                                                \
        __asm__ volatile("1: ldaxr %0, %3\n"                               \
                         "bic %1, %0, %4\n"                                \
                         "stlxr %w2, %1, %3\n"                             \
                         "cbnz %w2, 1b"                                    \
                         : "=&r"(old), "=&r"(newval), "=&r"(tmp), "+Q"(*ptr) \
                         : "r"(mask)                                       \
                         : "memory");                                      \
        return old;                                                        \
    }                                                                      \
    T __aarch64_ldset##W##_sync(T mask, volatile T *ptr) {                 \
        T old, newval, tmp;                                                \
        __asm__ volatile("1: ldaxr %0, %3\n"                               \
                         "orr %1, %0, %4\n"                                \
                         "stlxr %w2, %1, %3\n"                             \
                         "cbnz %w2, 1b"                                    \
                         : "=&r"(old), "=&r"(newval), "=&r"(tmp), "+Q"(*ptr) \
                         : "r"(mask)                                       \
                         : "memory");                                      \
        return old;                                                        \
    }                                                                      \
    T __aarch64_ldeor##W##_sync(T mask, volatile T *ptr) {                 \
        T old, newval, tmp;                                                \
        __asm__ volatile("1: ldaxr %0, %3\n"                               \
                         "eor %1, %0, %4\n"                                \
                         "stlxr %w2, %1, %3\n"                             \
                         "cbnz %w2, 1b"                                    \
                         : "=&r"(old), "=&r"(newval), "=&r"(tmp), "+Q"(*ptr) \
                         : "r"(mask)                                       \
                         : "memory");                                      \
        return old;                                                        \
    }

DEFINE_HELPERS(1, uint8_t)
DEFINE_HELPERS(2, uint16_t)
DEFINE_HELPERS(4, uint32_t)
DEFINE_HELPERS(8, uint64_t)
