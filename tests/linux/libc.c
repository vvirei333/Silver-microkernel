/* Real static-glibc program: the end-to-end proof that a normal Linux
 * userspace binary starts on the personality — libc initialisation, stdio,
 * file descriptors, synthetic devices, directory enumeration and exit.
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
#include <sys/stat.h>
#include <sys/random.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    printf("hello from libc\n");
    fflush(stdout);

    /* read + fstat on a real VFS file */
    char buffer[64];
    int fd = open("/sfs/readme.txt", O_RDONLY);
    if (fd < 0) {
        perror("open");
        return 1;
    }
    struct stat info;
    if (fstat(fd, &info) != 0 || info.st_size != 8) {
        perror("fstat");
        return 2;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    close(fd);
    if (count <= 0) {
        perror("read");
        return 3;
    }
    buffer[count] = 0;
    if (count != 8 || memcmp(buffer, "readme!!", 8) != 0) {
        fprintf(stderr, "unexpected contents: %s\n", buffer);
        return 4;
    }
    write(1, buffer, (size_t)count);
    write(1, "\n", 1);

    /* dup/dup2 + a writable device: this line must not reach the console */
    int saved = dup(1);
    int nullfd = open("/dev/null", O_WRONLY);
    if (saved < 0 || nullfd < 0) {
        perror("dup/open(/dev/null)");
        return 5;
    }
    if (dup2(nullfd, 1) != 1) {
        perror("dup2");
        return 6;
    }
    printf("this line goes to /dev/null\n");
    fflush(stdout);
    if (dup2(saved, 1) != 1) {
        perror("dup2 restore");
        return 7;
    }
    close(saved);
    close(nullfd);

    /* /dev/urandom: two reads must not repeat, and getrandom(2) must agree
       with the same pool (the old ticks-based xorshift was predictable). */
    int randomfd = open("/dev/urandom", O_RDONLY);
    unsigned char noise[8];
    unsigned char noise2[8];
    if (randomfd < 0 || read(randomfd, noise, sizeof(noise)) != (ssize_t)sizeof(noise)) {
        perror("/dev/urandom");
        return 8;
    }
    if (read(randomfd, noise2, sizeof(noise2)) != (ssize_t)sizeof(noise2)
        || memcmp(noise, noise2, sizeof(noise)) == 0) {
        fprintf(stderr, "urandom repeated\n");
        return 9;
    }
    close(randomfd);
    if (getrandom(noise2, sizeof(noise2), 0) != (ssize_t)sizeof(noise2)) {
        perror("getrandom");
        return 10;
    }
    printf("random ok\n");

    /* chdir/getcwd */
    char cwd[256];
    if (chdir("/sfs") != 0) {
        perror("chdir");
        return 9;
    }
    if (getcwd(cwd, sizeof(cwd)) == NULL || strcmp(cwd, "/sfs") != 0) {
        perror("getcwd");
        return 10;
    }

    /* nanosleep must actually block the task */
    struct timespec pause = {0, 10 * 1000 * 1000};
    if (nanosleep(&pause, NULL) != 0) {
        perror("nanosleep");
        return 11;
    }

    DIR *directory = opendir("/sfs");
    if (directory == NULL) {
        perror("opendir");
        return 12;
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
    return entries > 0 ? 0 : 13;
}
