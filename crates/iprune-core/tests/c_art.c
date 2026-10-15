/* Helper for the artwork tests. Uses only libgpod and gdk-pixbuf.
 * add <mp>:  gives every track except dbid 0x9999
 *            a solid cover, a colour of its own (the colour follows the track's place), then writes
 *            with itdb_write, which creates ArtworkDB and the F*.ithmb files.
 * show <mp>: one line per track, "<dbid hex> <title> <RGB hex>|NONE", the
 *            centre pixel (hex RGB) of its thumbnail, read back through ArtworkDB. */
#include <gpod/itdb.h>
#include <gdk-pixbuf/gdk-pixbuf.h>
#include <stdio.h>
#include <string.h>

/* A colour of its own per index: libgpod stores identical images once and
 * shares them between tracks, which would hide a lost cover. */
static guint32 colour(int i) {
    return ((guint32)((i * 37 + 11) & 0xff) << 24) | ((guint32)((i * 91 + 5) & 0xff) << 16) |
           ((guint32)((i * 53 + 101) & 0xff) << 8) | 0xff;
}

static int add(const char *mp) {
    GError *err = NULL;
    Itdb_iTunesDB *db = itdb_parse(mp, &err);
    if (!db) { fprintf(stderr, "parse: %s\n", err ? err->message : "?"); return 1; }
    if (!itdb_device_supports_artwork(db->device)) { fprintf(stderr, "no artwork support\n"); return 1; }
    int i = 0;
    for (GList *l = db->tracks; l; l = l->next) {
        Itdb_Track *t = l->data;
        if (t->dbid == 0x9999) continue;
        GdkPixbuf *pb = gdk_pixbuf_new(GDK_COLORSPACE_RGB, FALSE, 8, 300, 300);
        gdk_pixbuf_fill(pb, colour(i));
        if (!itdb_track_set_thumbnails_from_pixbuf(t, pb)) { fprintf(stderr, "thumbnails\n"); return 1; }
        g_object_unref(pb);
        i++;
    }
    if (!itdb_write(db, &err)) { fprintf(stderr, "write: %s\n", err ? err->message : "?"); return 1; }
    itdb_free(db);
    return 0;
}

static int show(const char *mp) {
    GError *err = NULL;
    Itdb_iTunesDB *db = itdb_parse(mp, &err);
    if (!db) { fprintf(stderr, "parse: %s\n", err ? err->message : "?"); return 1; }
    for (GList *l = db->tracks; l; l = l->next) {
        Itdb_Track *t = l->data;
        GdkPixbuf *pb = itdb_track_has_thumbnails(t) ? itdb_track_get_thumbnail(t, -1, -1) : NULL;
        if (!pb) { printf("%llx %s NONE\n", (unsigned long long)t->dbid, t->title); continue; }
        int w = gdk_pixbuf_get_width(pb), h = gdk_pixbuf_get_height(pb);
        int rs = gdk_pixbuf_get_rowstride(pb), nc = gdk_pixbuf_get_n_channels(pb);
        guchar *p = gdk_pixbuf_get_pixels(pb) + (h / 2) * rs + (w / 2) * nc;
        printf("%llx %s %02x%02x%02x\n", (unsigned long long)t->dbid, t->title, p[0], p[1], p[2]);
        g_object_unref(pb);
    }
    itdb_free(db);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && !strcmp(argv[1], "add")) return add(argv[2]);
    if (argc == 3 && !strcmp(argv[1], "show")) return show(argv[2]);
    fprintf(stderr, "usage: add|show|lose <mountpoint>\n");
    return 2;
}
