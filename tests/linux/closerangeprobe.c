#include <errno.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    /* What g_spawn does in the child: close everything above stderr. */
    errno = 0;
    long result = syscall(SYS_close_range, 3, ~0U, 0);
    printf("close_range=%ld errno=%d\n", result, errno);
    return result == 0 ? 0 : 1;
}
