/* Minimal GTK4 window: proves the GTK4/libadwaita stack loads in the guest. */
#include <gtk/gtk.h>

static gboolean quit_window(gpointer data) {
    g_application_quit(G_APPLICATION(data));
    return G_SOURCE_REMOVE;
}

static void activate(GtkApplication *app, gpointer data) {
    (void)data;
    GtkWidget *window = gtk_application_window_new(app);
    gtk_window_set_title(GTK_WINDOW(window), "Werewolf GTK4");
    gtk_window_set_default_size(GTK_WINDOW(window), 240, 120);
    GtkWidget *label = gtk_label_new("GTK4 on WerewolfOS");
    gtk_window_set_child(GTK_WINDOW(window), label);
    gtk_widget_set_visible(window, TRUE);
    g_timeout_add(2000, quit_window, app);
}

int main(int argc, char **argv) {
    GtkApplication *app =
        gtk_application_new("org.werewolf.gtk4hello", G_APPLICATION_NON_UNIQUE);
    g_signal_connect(app, "activate", G_CALLBACK(activate), NULL);
    int status = g_application_run(G_APPLICATION(app), argc, argv);
    g_object_unref(app);
    return status;
}
