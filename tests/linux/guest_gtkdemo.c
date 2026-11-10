/* GTK3 demo used by the in-guest toolkit probe (slice 61).
 *
 * Not part of tools/linux-test.sh: it needs a full GTK3 sysroot, which the
 * 64 MiB base image cannot hold. It is built against the host's GTK headers
 * (same 3.24.52) and linked against the extracted guest libraries, then
 * injected as /sfs/gtkdemo next to /sfs/gtk-serve:
 *
 *   aarch64-linux-gnu-gcc -O2 $(pkg-config --cflags gtk+-3.0) \
 *     -o gtkdemo guest_gtkdemo.c -L$ROOT -Wl,-rpath-link,$ROOT \
 *     -Wl,--allow-shlib-undefined -l:libgtk-3.so.0 -l:libgdk-3.so.0 \
 *     -l:libgobject-2.0.so.0 -l:libglib-2.0.so.0 -l:libpango-1.0.so.0 \
 *     -l:libcairo.so.2 -l:libgdk_pixbuf-2.0.so.0 -l:libatk-1.0.so.0
 *   patchelf --set-interpreter /sfs/ld-linux-aarch64.so.1 \
 *     --set-rpath '$ORIGIN' gtkdemo
 *
 * Expected guest output: "gtk window shown", "gtk-serve: shm frame
 * committed", "gtk mainloop ok", "gtk client ok".
 */
#include <gtk/gtk.h>

static gboolean quit_window(gpointer data) {
    (void)data;
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

int main(int argc, char **argv) {
    gtk_init(&argc, &argv);
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "WerewolfOS GTK");
    gtk_window_set_default_size(GTK_WINDOW(window), 320, 200);
    /* No client-side titlebar: the mini compositor has no icon theme and
       gdk-pixbuf in this image has no raster loader yet. */
    gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
    GtkWidget *label = gtk_label_new("WerewolfOS GTK demo");
    gtk_container_add(GTK_CONTAINER(window), label);
    g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);
    gtk_widget_show_all(window);
    g_print("gtk window shown\n");
    g_timeout_add(2500, quit_window, NULL);
    gtk_main();
    g_print("gtk mainloop ok\n");
    return 0;
}
