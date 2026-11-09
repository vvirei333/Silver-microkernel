/* memfd + ftruncate + writable MAP_SHARED across fork.
 *
 *   linuxrun /sfs/linux-memfd
 *
 * Expected: "shared memfd ok" and exit 0.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <sys/mman.h>
#include <unistd.h>

static int memfd_create_compat(const char *name, unsigned int flags) {
    register long x8 __asm__("x8") = 279;
    register long x0 __asm__("x0") = (long)name;
    register long x1 __asm__("x1") = flags;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1) : "memory");
    return (int)x0;
}

int main(void) {
    int fd = memfd_create_compat("buffer", 0);
    if (fd < 0) {
        perror("memfd_create");
        return 2;
    }
    if (ftruncate(fd, 4096) != 0) {
        perror("ftruncate");
        return 3;
    }
    char *shared = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (shared == MAP_FAILED) {
        perror("mmap");
        return 4;
    }

    /* Mapping -> file. */
    memcpy(shared, "mapping", 8);
    char check[16] = {0};
    if (pread(fd, check, 8, 0) != 8 || memcmp(check, "mapping", 8) != 0) {
        printf("mapping not visible through read\n");
        return 5;
    }

    /* File -> mapping. */
    if (pwrite(fd, "FILE", 4, 0) != 4 || memcmp(shared, "FILE", 4) != 0) {
        printf("write not visible through mapping\n");
        return 6;
    }

    /* The child maps the inherited descriptor at the same shared frames. */
    pid_t child = fork();
    if (child == 0) {
        char *window = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (window == MAP_FAILED) {
            _exit(3);
        }
        window[64] = 'C';
        memcpy(window + 128, "child", 6);
        _exit(munmap(window, 4096) == 0 ? 0 : 4);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        printf("child failed\n");
        return 7;
    }
    if (shared[64] != 'C' || memcmp(shared + 128, "child", 6) != 0) {
        printf("child write not shared\n");
        return 8;
    }

    /* Unmapping the window must not free the memfd's frames. */
    if (munmap(shared, 4096) != 0) {
        return 9;
    }
    char *again = mmap(0, 4096, PROT_READ, MAP_SHARED, fd, 0);
    if (again == MAP_FAILED || again[64] != 'C' || memcmp(again + 128, "child", 6) != 0) {
        printf("remap lost data\n");
        return 10;
    }
    munmap(again, 4096);
    close(fd);

    printf("shared memfd ok\n");
    return 0;
}
