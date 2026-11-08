/* Real static-glibc program: the end-to-end proof that a normal Linux
 * userspace binary starts on the personality (libc initialisation, stdio,
 * exit).
 *
 *   linuxrun /sfs/linux-libc
 *
 * Expected: "hello from libc" and exit status 11. Builds only when the cross
 * compiler ships a static libc; tools/linux-test.sh skips it otherwise.
 */
#include <stdio.h>

int main(void) {
    printf("hello from libc\n");
    return 11;
}
