/* Long SFS names (up to 255 bytes).
 *
 *   linuxrun /sfs/linux-longname
 *
 * `tools/linux-test.sh` installs a file whose name is 72 bytes, far past the
 * old 27-byte directory-entry limit. The kernel keeps the 32-byte entry and
 * stores the name in its own data block (length byte 0xFF), so open, stat and
 * readdir must all follow it.
 *
 * Expected: "long name ok" and exit 0.
 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define LONG_NAME \
    "alpine-package-version-3.24.4-r0-aarch64-long-name-test-file.txt"

static int failed(const char *message) {
    perror(message);
    return 1;
}

int main(void) {
    const char *path = "/sfs/" LONG_NAME;
    int fd = open(path, O_RDONLY);
    if (fd < 0) return failed("open long name");

    struct stat status;
    if (fstat(fd, &status) != 0 || status.st_size != 12) return failed("fstat");

    char buffer[16] = {0};
    if (read(fd, buffer, 12) != 12 || memcmp(buffer, "long name ok", 12) != 0) {
        return failed("read");
    }
    close(fd);

    /* The listing path decodes the overflow block too. */
    DIR *dir = opendir("/sfs");
    if (!dir) return failed("opendir /sfs");
    int found = 0;
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (strcmp(entry->d_name, LONG_NAME) == 0) found = 1;
    }
    closedir(dir);
    if (!found) {
        fprintf(stderr, "long name missing from readdir\n");
        return 1;
    }

    printf("long name ok\n");
    return 0;
}
