/* Writable /tmp from the Linux personality.
 *
 *   linuxrun /sfs/linux-scratch
 *
 * Expected: "scratch ok" and exit 0.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    const char *runtime = getenv("XDG_RUNTIME_DIR");
    if (runtime == 0 || strcmp(runtime, "/tmp") != 0) {
        return 17;
    }
    if (mkdir("/tmp/wayland-0", 0700) != 0 && errno != EEXIST) {
        perror("mkdir");
        return 2;
    }

    int fd = open("/tmp/wtest.lock", O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) {
        perror("create lock");
        return 3;
    }
    errno = 0;
    if (open("/tmp/wtest.lock", O_CREAT | O_EXCL | O_RDWR, 0600) != -1 ||
        errno != EEXIST) {
        return 4;
    }
    if (write(fd, "lock", 4) != 4) {
        return 5;
    }
    lseek(fd, 0, SEEK_SET);
    char buffer[8] = {0};
    if (read(fd, buffer, 4) != 4 || memcmp(buffer, "lock", 4) != 0) {
        return 6;
    }
    close(fd);

    /* Reopening without O_TRUNC preserves the contents. */
    fd = open("/tmp/wtest.lock", O_RDWR);
    if (fd < 0 || lseek(fd, 0, SEEK_END) != 4 || write(fd, "!", 1) != 1) {
        return 7;
    }
    close(fd);
    struct stat status;
    if (stat("/tmp/wtest.lock", &status) != 0 || status.st_size != 5) {
        return 8;
    }

    /* rename(2) replaces the destination. */
    int temporary = open("/tmp/wtest.tmp", O_CREAT | O_RDWR | O_TRUNC, 0600);
    if (temporary < 0 || write(temporary, "new", 3) != 3) {
        return 9;
    }
    close(temporary);
    if (rename("/tmp/wtest.tmp", "/tmp/wtest.lock") != 0) {
        perror("rename");
        return 10;
    }
    fd = open("/tmp/wtest.lock", O_RDONLY);
    if (fd < 0 || read(fd, buffer, 4) != 3 || memcmp(buffer, "new", 3) != 0) {
        return 11;
    }
    close(fd);

    /* A file inside the runtime directory, then clean up. */
    fd = open("/tmp/wayland-0/display.lock", O_CREAT | O_RDWR, 0600);
    if (fd < 0 || write(fd, "1", 1) != 1) {
        return 12;
    }
    close(fd);
    if (unlink("/tmp/wtest.lock") != 0) {
        return 13;
    }
    if (unlink("/tmp/wayland-0/display.lock") != 0) {
        return 14;
    }
    if (rmdir("/tmp/wayland-0") != 0) {
        return 15;
    }

    /* SFS stays read-only for the Linux personality. */
    errno = 0;
    if (open("/sfs/ro-test", O_CREAT | O_WRONLY, 0600) != -1 || errno != EROFS) {
        return 16;
    }

    printf("scratch ok\n");
    return 0;
}
