/* musl-style stdio on the Linux ABI.
 *
 *   linuxrun /sfs/linux-stdio
 *
 * musl's stdio does not call `write`/`read` directly: every stream goes
 * through `writev`/`readv`. When the kernel only accepted the console for
 * `writev` (and had no `readv` at all), OpenRC's `fdopen`/`fprintf` of the
 * service dependency tree silently produced a zero-byte file. `fdopen` also
 * cross-checks `fcntl(F_GETFL)`, so the access mode must survive `open`.
 *
 * Expected: "stdio ok" and exit 0.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/uio.h>
#include <unistd.h>

static int failed(const char *message) {
    perror(message);
    return 1;
}

int main(void) {
    /* writev on a regular file, not just the console. */
    int fd = open("/tmp/stdio-v", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (fd < 0) return failed("open writev");
    struct iovec out[2] = {{"hello ", 6}, {"writev", 6}};
    if (writev(fd, out, 2) != 12) return failed("writev");
    if ((fcntl(fd, F_GETFL) & O_ACCMODE) != O_WRONLY) return failed("F_GETFL");
    close(fd);

    /* readv gathers into both iovecs. */
    fd = open("/tmp/stdio-v", O_RDONLY);
    if (fd < 0) return failed("open readv");
    if ((fcntl(fd, F_GETFL) & O_ACCMODE) != O_RDONLY) return failed("F_GETFL read");
    char first[6] = {0};
    char second[6] = {0};
    struct iovec in[2] = {{first, 6}, {second, 6}};
    if (readv(fd, in, 2) != 12) return failed("readv");
    close(fd);
    if (memcmp(first, "hello ", 6) != 0 || memcmp(second, "writev", 6) != 0) {
        return failed("readv bytes");
    }

    /* The OpenRC deptree shape: fdopen over an O_WRONLY descriptor. */
    fd = open("/tmp/stdio-f", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (fd < 0) return failed("open fdopen");
    FILE *file = fdopen(fd, "w");
    if (!file) return failed("fdopen write");
    for (int i = 0; i < 50; i++) fprintf(file, "line %d\n", i);
    if (fclose(file) != 0) return failed("fclose write");

    fd = open("/tmp/stdio-f", O_RDONLY);
    if (fd < 0) return failed("open fdopen read");
    file = fdopen(fd, "r");
    if (!file) return failed("fdopen read");
    int lines = 0;
    char line[32];
    while (fgets(line, sizeof line, file)) lines++;
    fclose(file);
    if (lines != 50) return failed("fdopen lines");

    unlink("/tmp/stdio-v");
    unlink("/tmp/stdio-f");
    printf("stdio ok\n");
    return 0;
}
