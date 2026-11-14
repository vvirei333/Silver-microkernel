/*
 * GIO's GDesktopAppInfo does not fork the application itself: it execs
 * `gio-launch-desktop` with the program and its arguments, because setting
 * GIO_LAUNCHED_DESKTOP_FILE_PID is unsafe after a threaded fork. A normal
 * desktop has this helper in libexec; this is our copy, pointed at by
 * GIO_LAUNCH_DESKTOP.
 *
 * The helper is deliberately tiny: record the pid, then exec the program.
 */

#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

int main(int argc, char *argv[]) {
    if (argc < 2) {
        return 1;
    }
    char buffer[64];
    int length = snprintf(buffer, sizeof(buffer),
                          "GIO_LAUNCHED_DESKTOP_FILE_PID=%ld", (long)getpid());
    if (length > 0 && (unsigned long)length < sizeof(buffer)) {
        if (putenv(buffer) != 0) {
            return 1;
        }
    }
    execvp(argv[1], argv + 1);
    return 127;
}
