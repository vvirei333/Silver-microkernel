/* Real GLib/GObject under the Linux ABI.
 *
 *   linuxrun /sfs/glibtest
 *
 * Prints the GLib version, instantiates a GObject subclass, runs a
 * GMainLoop with a timeout source, then prints "glib mainloop ok".
 */
#include <glib-object.h>
#include <glib.h>
#include <stdio.h>

typedef struct {
    GObject parent_instance;
} Wolf;

typedef struct {
    GObjectClass parent_class;
} WolfClass;

G_DEFINE_TYPE(Wolf, wolf, G_TYPE_OBJECT)

static void wolf_class_init(WolfClass *klass) {
    (void)klass;
}

static void wolf_init(Wolf *self) {
    (void)self;
}

static gboolean quit_loop(gpointer data) {
    g_main_loop_quit((GMainLoop *)data);
    return G_SOURCE_REMOVE;
}

int main(void) {
    g_print("glib %d.%d.%d\n", GLIB_MAJOR_VERSION, GLIB_MINOR_VERSION,
            GLIB_MICRO_VERSION);
    Wolf *wolf = g_object_new(wolf_get_type(), NULL);
    if (wolf == NULL || !G_IS_OBJECT(wolf)) {
        g_print("gobject failed\n");
        return 1;
    }
    gchar *name = g_strdup_printf("wolf-%u", 42u);
    g_print("gobject %s\n", name);
    g_free(name);
    g_object_unref(wolf);

    GMainLoop *loop = g_main_loop_new(NULL, FALSE);
    g_timeout_add(10, quit_loop, loop);
    g_main_loop_run(loop);
    g_main_loop_unref(loop);
    g_print("glib mainloop ok\n");
    return 0;
}
