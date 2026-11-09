/* Real GIO: write a file through GFile, read it back, then a GMainLoop. */
#include <gio/gio.h>
#include <glib.h>
#include <string.h>

static gboolean quit_loop(gpointer data) {
    g_main_loop_quit((GMainLoop *)data);
    return G_SOURCE_REMOVE;
}

int main(void) {
    GError *error = NULL;
    const char text[] = "gio file ok\n";
    if (!g_file_set_contents("/tmp/gio-test.txt", text, sizeof(text) - 1,
                             &error)) {
        g_print("gio write failed: %s\n", error->message);
        return 1;
    }
    char *contents = NULL;
    gsize length = 0;
    if (!g_file_get_contents("/tmp/gio-test.txt", &contents, &length, &error)) {
        g_print("gio read failed: %s\n", error->message);
        return 1;
    }
    if (length != sizeof(text) - 1 || memcmp(contents, text, length) != 0) {
        g_print("gio content mismatch\n");
        return 1;
    }
    g_print("gio %s", contents);
    g_free(contents);

    GFile *file = g_file_new_for_path("/tmp/gio-test.txt");
    char *path = g_file_get_path(file);
    g_print("gio path %s\n", path);
    g_free(path);
    g_object_unref(file);

    GMainLoop *loop = g_main_loop_new(NULL, FALSE);
    g_timeout_add(10, quit_loop, loop);
    g_main_loop_run(loop);
    g_main_loop_unref(loop);
    g_print("gio mainloop ok\n");
    return 0;
}
