/* Real static-glibc program: the end-to-end proof that a normal Linux
 * userspace binary starts on the personality — libc initialisation, stdio,
 * open/read/close, directory enumeration and exit.
 *
 *   linuxrun /sfs/linux-libc
 *
 * Expected: "hello from libc", "readme!!", "entries=..." and status 0.
 * Builds only when the cross compiler ships a static libc;
 * tools/linux-test.sh skips it otherwise.
 */
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    printf("hello from libc\n");

    char buffer[64];
    int fd = open("/sfs/readme.txt", O_RDONLY);
    if (fd < 0) {
        perror("open");
        return 1;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    close(fd);
    if (count <= 0) {
        perror("read");
        return 2;
    }
    buffer[count] = 0;
    if (count != 8 || memcmp(buffer, "readme!!", 8) != 0) {
        fprintf(stderr, "unexpected contents: %s\n", buffer);
        return 3;
    }
    write(1, buffer, (size_t)count);
    write(1, "\n", 1);

    DIR *directory = opendir("/sfs");
    if (directory == NULL) {
        perror("opendir");
        return 4;
    }
    int entries = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        if (entry->d_name[0] != '.') {
            entries++;
        }
    }
    closedir(directory);
    printf("entries=%d\n", entries);
    return entries > 0 ? 0 : 5;
}
