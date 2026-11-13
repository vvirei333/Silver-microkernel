/* WerewolfOS terminal: a GTK3 window that runs busybox applets.
 *
 * Phosh (and therefore this image) is GTK3 + libhandy, so the terminal is a
 * GTK3 application as well. Commands are executed synchronously through
 * `busybox sh -c`; output and exit status are appended to the text view. A
 * real PTY with job control is the next step, this is the "usable now" cut. */
#include <gtk/gtk.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static GtkWidget *view;
static GtkTextBuffer *buffer;
static GtkWidget *entry;
static char cwd[PATH_MAX] = "/tmp";

static void append_text(const char *text) {
    if (text == NULL || *text == '\0') {
        return;
    }
    GtkTextIter end;
    gtk_text_buffer_get_end_iter(buffer, &end);
    gtk_text_buffer_insert(buffer, &end, text, -1);
    gtk_text_buffer_get_end_iter(buffer, &end);
    gtk_text_buffer_place_cursor(buffer, &end);
    GtkTextMark *mark = gtk_text_buffer_create_mark(buffer, NULL, &end, FALSE);
    gtk_text_view_scroll_mark_onscreen(GTK_TEXT_VIEW(view), mark);
    gtk_text_buffer_delete_mark(buffer, mark);
}

static void change_directory(const char *argument) {
    char target[PATH_MAX];
    const char *arg = argument;
    while (*arg == ' ') {
        arg++;
    }
    if (*arg == '\0') {
        snprintf(target, sizeof(target), "/tmp");
    } else if (*arg == '/') {
        snprintf(target, sizeof(target), "%s", arg);
    } else {
        snprintf(target, sizeof(target), "%s/%s", cwd, arg);
    }
    char resolved[PATH_MAX];
    if (realpath(target, resolved) == NULL) {
        char message[PATH_MAX + 32];
        snprintf(message, sizeof(message), "cd: %s: no such directory\n", target);
        append_text(message);
        return;
    }
    snprintf(cwd, sizeof(cwd), "%s", resolved);
}

static void run_command(const char *line) {
    char prompt[PATH_MAX + 600];
    snprintf(prompt, sizeof(prompt), "%s $ %s\n", cwd, line);
    append_text(prompt);

    if (strcmp(line, "clear") == 0) {
        gtk_text_buffer_set_text(buffer, "", -1);
        return;
    }
    if (strncmp(line, "cd", 2) == 0 && (line[2] == '\0' || line[2] == ' ')) {
        change_directory(line + 2);
        return;
    }

    const char *argv[] = {"/sfs/busybox", "sh", "-c", line, NULL};
    gchar **environment = g_get_environ();
    environment = g_environ_setenv(environment, "PATH",
                                   "/sfs:/sfs/bin:/tmp", TRUE);
    environment = g_environ_setenv(environment, "HOME", "/tmp", TRUE);
    environment = g_environ_setenv(environment, "PWD", cwd, TRUE);
    environment = g_environ_setenv(environment, "TERM", "dumb", TRUE);

    gchar *output = NULL;
    gchar *error_output = NULL;
    gint status = 0;
    GError *error = NULL;
    gboolean spawned = g_spawn_sync(cwd, (gchar **)argv, environment,
                                    G_SPAWN_SEARCH_PATH, NULL, NULL, &output,
                                    &error_output, &status, &error);
    g_strfreev(environment);
    if (!spawned) {
        char message[600];
        snprintf(message, sizeof(message), "cannot run: %s\n",
                 error != NULL ? error->message : "unknown error");
        append_text(message);
        g_clear_error(&error);
        return;
    }
    append_text(output);
    append_text(error_output);
    g_free(output);
    g_free(error_output);
    if (!g_spawn_check_wait_status(status, &error)) {
        char message[600];
        snprintf(message, sizeof(message), "[%s]\n",
                 error != NULL ? error->message : "command failed");
        append_text(message);
        g_clear_error(&error);
    }
}

static void on_entry_activate(GtkEntry *widget, gpointer data) {
    (void)data;
    char line[1024];
    snprintf(line, sizeof(line), "%s", gtk_entry_get_text(widget));
    gtk_entry_set_text(widget, "");
    run_command(line);
}

static void on_activate(GtkApplication *app, gpointer data) {
    (void)data;
    GtkWidget *window = gtk_application_window_new(app);
    gtk_window_set_title(GTK_WINDOW(window), "Terminal");
    gtk_window_set_default_size(GTK_WINDOW(window), 1024, 768);

    GtkWidget *header = gtk_header_bar_new();
    gtk_header_bar_set_title(GTK_HEADER_BAR(header), "Terminal");
    gtk_window_set_titlebar(GTK_WINDOW(window), header);

    GtkWidget *box = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
    gtk_container_add(GTK_CONTAINER(window), box);

    view = gtk_text_view_new();
    gtk_text_view_set_editable(GTK_TEXT_VIEW(view), FALSE);
    gtk_text_view_set_cursor_visible(GTK_TEXT_VIEW(view), FALSE);
    gtk_text_view_set_monospace(GTK_TEXT_VIEW(view), TRUE);
    gtk_text_view_set_wrap_mode(GTK_TEXT_VIEW(view), GTK_WRAP_WORD_CHAR);
    gtk_widget_set_margin_start(view, 12);
    gtk_widget_set_margin_end(view, 12);
    gtk_widget_set_margin_top(view, 8);
    gtk_widget_set_margin_bottom(view, 8);
    buffer = gtk_text_view_get_buffer(GTK_TEXT_VIEW(view));

    GtkWidget *scroller = gtk_scrolled_window_new(NULL, NULL);
    gtk_scrolled_window_set_policy(GTK_SCROLLED_WINDOW(scroller),
                                   GTK_POLICY_AUTOMATIC, GTK_POLICY_AUTOMATIC);
    gtk_container_add(GTK_CONTAINER(scroller), view);
    gtk_box_pack_start(GTK_BOX(box), scroller, TRUE, TRUE, 0);

    entry = gtk_entry_new();
    gtk_entry_set_placeholder_text(GTK_ENTRY(entry), "command");
    gtk_widget_set_margin_start(entry, 12);
    gtk_widget_set_margin_end(entry, 12);
    gtk_widget_set_margin_bottom(entry, 12);
    gtk_entry_set_has_frame(GTK_ENTRY(entry), TRUE);
    gtk_box_pack_start(GTK_BOX(box), entry, FALSE, FALSE, 0);
    g_signal_connect(entry, "activate", G_CALLBACK(on_entry_activate), NULL);

    append_text("WerewolfOS terminal\n"
                "Type a command and press Enter. Try: ls /sfs, cat "
                "/etc/passwd, uname -a, free, ps.\n\n");

    gtk_widget_show_all(window);
    gtk_window_present(GTK_WINDOW(window));
    gtk_widget_grab_focus(entry);
}

int main(int argc, char **argv) {
    GtkApplication *app = gtk_application_new("org.werewolf.terminal",
                                              G_APPLICATION_NON_UNIQUE);
    g_signal_connect(app, "activate", G_CALLBACK(on_activate), NULL);
    int status = g_application_run(G_APPLICATION(app), argc, argv);
    g_object_unref(app);
    return status;
}
