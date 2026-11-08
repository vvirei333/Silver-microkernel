// Newlib syscall stubs for the guest browser engine.
//
// The C/C++ side (litehtml, gumbo, libstdc++) expects a hosted C library.
// Newlib supplies malloc, string functions and the C++ runtime support; the
// seven functions below connect it to the Silver syscall ABI. The backing
// Rust functions live in wolfser.rs.

#include <errno.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/times.h>
#include <time.h>

extern void   wolf_sys_write(const void* data, size_t length);
extern void*  wolf_sys_mmap(size_t length);
extern void   wolf_sys_exit(uint64_t status) __attribute__((noreturn));
extern uint64_t wolf_sys_uptime_ms(void);

// Diagnostics: the browser task does not own the console, so SYS_PRINT is
// discarded. Encode the failing stage in the exit status instead; the kernel
// always prints that.
volatile int wolf_heap_failed = 0;
volatile int wolf_heap_full = 0;


// The engine heap. One contiguous mmap keeps newlib's sbrk contract simple;
// the Rust allocator uses SYS_BRK and is deliberately a separate arena.
#define WOLF_HEAP_BYTES (20u * 1024u * 1024u)

static unsigned char* heap_base = 0;
static size_t         heap_used = 0;

void* _sbrk(ptrdiff_t increment)
{
    if(heap_base == 0)
    {
        heap_base = (unsigned char*) wolf_sys_mmap(WOLF_HEAP_BYTES);
        if(heap_base == 0)
        {
            wolf_heap_failed = 1;
            errno = ENOMEM;
            return (void*) -1;
        }
    }
    if(increment == 0)
    {
        return heap_base + heap_used;
    }
    if(increment < 0)
    {
        const size_t amount = (size_t) (-increment);
        if(amount > heap_used)
        {
            errno = ENOMEM;
            return (void*) -1;
        }
        heap_used -= amount;
        return heap_base + heap_used;
    }
    const size_t amount = (size_t) increment;
    if(heap_used + amount > WOLF_HEAP_BYTES)
    {
        wolf_heap_full = 1;
        errno = ENOMEM;
        return (void*) -1;
    }
    void* previous = heap_base + heap_used;
    heap_used += amount;
    return previous;
}

void _exit(int status)
{
    wolf_sys_exit((uint64_t) status);
    __builtin_unreachable();
}

// Newlib's abort() would exit silently. Route assertions and C++ runtime
// failures through a distinct status so the serial log tells them apart from
// a normal browser exit.
void abort(void)
{
    // Return address identifies the exact throw/abort site. The kernel prints
    // the full 64-bit status, so addr2line/nm can map it back to a symbol.
    uint64_t flags = (uint64_t) (wolf_heap_failed ? 1 : 0) |
                     (uint64_t) (wolf_heap_full ? 2 : 0);
    wolf_sys_exit(0xAB00000000000000ull |
                  (flags << 48) |
                  (uint64_t) __builtin_return_address(0));
    __builtin_unreachable();
}

// Newlib assertions would abort via abort(); keep the failing source line in
// the exit status so a serial log identifies the exact litehtml check.
void __assert_func(const char* file, int line, const char* function, const char* expression)
{
    (void) file;
    (void) function;
    (void) expression;
    wolf_sys_exit(0xAC00000000000000ull | ((uint64_t) line & 0xFFFF));
    __builtin_unreachable();
}

int _write(int file, const void* buffer, size_t length)
{
    if(file == 1 || file == 2)
    {
        wolf_sys_write(buffer, length);
        return (int) length;
    }
    return -1;
}

int _read(int file, void* buffer, size_t length)
{
    (void) file;
    (void) buffer;
    (void) length;
    return -1;
}

int _close(int file)
{
    (void) file;
    return -1;
}

int _fstat(int file, struct stat* status)
{
    (void) file;
    if(status != 0)
    {
        status->st_mode = S_IFCHR;
    }
    return 0;
}

int _isatty(int file)
{
    (void) file;
    return 1;
}

int _lseek(int file, int offset, int whence)
{
    (void) file;
    (void) offset;
    (void) whence;
    return 0;
}

int _kill(int pid, int signal)
{
    (void) pid;
    (void) signal;
    return -1;
}

int _getpid(void)
{
    return 1;
}

int _gettimeofday(struct timeval* time, void* timezone)
{
    (void) timezone;
    if(time != 0)
    {
        const uint64_t millis = wolf_sys_uptime_ms();
        time->tv_sec          = (time_t) (millis / 1000);
        time->tv_usec         = (suseconds_t) ((millis % 1000) * 1000);
    }
    return 0;
}

clock_t _times(struct tms* buffer)
{
    const uint64_t millis = wolf_sys_uptime_ms();
    const clock_t ticks   = (clock_t) (millis / 10);
    if(buffer != 0)
    {
        buffer->tms_utime  = ticks;
        buffer->tms_stime  = 0;
        buffer->tms_cutime = 0;
        buffer->tms_cstime = 0;
    }
    return ticks;
}

// Newlib's __libc_init_array calls _init before walking .init_array. The
// linker script provides the array bounds; an empty _init is enough here.
void _init(void)
{
}

void _fini(void)
{
}
