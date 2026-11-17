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
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static int failed(const char *message) {
    perror(message);
    return 1;
}

int main(void) {
    /* Re-executed by the large-environment case below. */
    if (getenv("STDIO_CHILD")) {
        printf("large env ok\n");
        return 0;
    }

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

    /* sendfile must copy and advance the input offset (busybox cp). */
    int source = open("/tmp/stdio-sf-in", O_CREAT | O_TRUNC | O_RDWR, 0600);
    int sink = open("/tmp/stdio-sf-out", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (source < 0 || sink < 0) return failed("open sendfile");
    if (write(source, "sendfile data", 13) != 13 || lseek(source, 0, SEEK_SET) != 0) {
        return failed("prepare sendfile");
    }
    if (sendfile(sink, source, NULL, 13) != 13) return failed("sendfile");
    char copied[16] = {0};
    close(sink);
    sink = open("/tmp/stdio-sf-out", O_RDONLY);
    if (sink < 0 || read(sink, copied, 13) != 13 || memcmp(copied, "sendfile data", 13)) {
        return failed("sendfile bytes");
    }
    if (read(source, copied, 1) != 0) return failed("sendfile offset");
    close(source);
    close(sink);

    /* copy_file_range with explicit offsets. */
    source = open("/tmp/stdio-sf-in", O_RDONLY);
    sink = open("/tmp/stdio-sf-out", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (source < 0 || sink < 0) return failed("open copy_file_range");
    loff_t input_offset = 0;
    loff_t output_offset = 0;
    if (copy_file_range(source, &input_offset, sink, &output_offset, 13, 0) != 13) {
        return failed("copy_file_range");
    }
    if (input_offset != 13 || output_offset != 13) return failed("copy_file_range offsets");
    close(source);
    close(sink);

    /* pwrite past EOF zero-fills the hole (dconf writes byte 1 of a fresh
     * file and expects a two-byte file). */
    int hole = open("/tmp/stdio-hole", O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (hole < 0) return failed("open hole");
    if (pwrite(hole, "x", 1, 1) != 1) return failed("pwrite past EOF");
    if (lseek(hole, 0, SEEK_CUR) != 0) return failed("pwrite moved offset");
    struct stat hole_status;
    if (fstat(hole, &hole_status) != 0 || hole_status.st_size != 2) {
        return failed("pwrite size");
    }
    char hole_bytes[2] = {0};
    if (pread(hole, hole_bytes, 2, 0) != 2 || hole_bytes[0] != 0 || hole_bytes[1] != 'x') {
        return failed("pwrite hole bytes");
    }
    close(hole);

    unlink("/tmp/stdio-v");
    unlink("/tmp/stdio-f");
    unlink("/tmp/stdio-sf-in");
    unlink("/tmp/stdio-sf-out");
    unlink("/tmp/stdio-hole");

    /* An environment larger than the native 32-entry spawn cap must exec. */
    pid_t child = fork();
    if (child == 0) {
        char *child_env[49];
        static char slots[45][16];
        int index = 0;
        for (; index < 45; index++) {
            snprintf(slots[index], sizeof slots[index], "STDIO_VAR_%d=x", index);
            child_env[index] = slots[index];
        }
        child_env[45] = "STDIO_CHILD=1";
        child_env[46] = "STDIO_CHILD2=1";
        child_env[47] = "STDIO_CHILD3=1";
        child_env[48] = NULL;
        execve("/sfs/linux-stdio", (char *const[]){"/sfs/linux-stdio", 0}, child_env);
        _exit(1);
    }
    int status = 0;
    if (child < 0 || waitpid(child, &status, 0) != child) return failed("waitpid");
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return failed("large env exec");

    printf("stdio ok\n");
    return 0;
}
