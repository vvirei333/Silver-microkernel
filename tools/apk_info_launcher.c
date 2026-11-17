/* Static launcher: `linuxrun` takes one path and no arguments, so this is how
 * the guest runs `apk --no-network info` and proves the Alpine package
 * database and keys are readable under the Linux ABI. */
#include <unistd.h>

int main(void) {
    char *args[] = {"/sbin/apk", "--no-network", "info", 0};
    execv("/sbin/apk", args);
    return 127;
}
