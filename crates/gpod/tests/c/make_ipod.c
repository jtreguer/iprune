/* make_ipod <out iTunesDB> <manifest>: write an iTunesDB described by a
   manifest, for the adoption tests. Uses only libgpod.

   Manifest: one record per line, fields separated by TAB. Empty tag fields
   are left unset (NULL), as on an iPod that never got the tag; the literal
   fields <empty> and <space> give an empty string and a blank one.

     M <name>                                  master playlist (the iPod name)
     T <dbid hex> <size> <tracklen ms> <title> <artist> <album> <playcount>
     P <name> <dbid hex>,<dbid hex>,...        regular playlist, members in order
     S <name> <dbid hex>,...                   smart playlist
*/
#include <gpod/itdb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MAXF 10

static int split(char *line, char **f) {
    int n = 0;
    f[n++] = line;
    for (char *p = line; *p && n < MAXF; p++) {
        if (*p == '\t') { *p = 0; f[n++] = p + 1; }
    }
    return n;
}

/* An empty field is NULL; "<empty>" is the empty string and "<space>" a
   single space, which libgpod keeps as they are. */
static char *opt(char **f, int n, int i) {
    if (i >= n || !f[i][0]) return NULL;
    if (!strcmp(f[i], "<empty>")) return g_strdup("");
    if (!strcmp(f[i], "<space>")) return g_strdup(" ");
    return g_strdup(f[i]);
}

static Itdb_Track *find(Itdb_iTunesDB *db, guint64 dbid) {
    for (GList *l = db->tracks; l; l = l->next)
        if (((Itdb_Track *)l->data)->dbid == dbid) return l->data;
    fprintf(stderr, "no track %llx\n", (unsigned long long)dbid);
    exit(1);
}

static void members(Itdb_iTunesDB *db, Itdb_Playlist *pl, const char *list) {
    char *copy = g_strdup(list);
    for (char *tok = strtok(copy, ","); tok; tok = strtok(NULL, ","))
        itdb_playlist_add_track(pl, find(db, strtoull(tok, NULL, 16)), -1);
    g_free(copy);
}

int main(int argc, char **argv) {
    if (argc != 3) { fprintf(stderr, "usage: make_ipod <out> <manifest>\n"); return 2; }
    FILE *in = fopen(argv[2], "r");
    if (!in) { perror("manifest"); return 2; }
    Itdb_iTunesDB *db = itdb_new();
    char name[] = "iPod de Test";
    Itdb_Playlist *mpl = itdb_playlist_new(name, FALSE);
    itdb_playlist_set_mpl(mpl);
    itdb_playlist_add(db, mpl, -1);

    char line[8192];
    int serial = 0;
    while (fgets(line, sizeof line, in)) {
        line[strcspn(line, "\r\n")] = 0;
        if (!line[0]) continue;
        char *f[MAXF];
        int n = split(line, f);
        if (!strcmp(f[0], "M") && n >= 2) {
            g_free(mpl->name);
            mpl->name = g_strdup(f[1]);
        } else if (!strcmp(f[0], "T") && n >= 4) {
            Itdb_Track *t = itdb_track_new();
            t->dbid = strtoull(f[1], NULL, 16);
            char path[64];
            snprintf(path, sizeof path, ":iPod_Control:Music:F%02d:T%04d.mp3", serial % 50, serial);
            serial++;
            t->ipod_path = g_strdup(path);
            t->size = (guint32)strtoul(f[2], NULL, 10);
            t->tracklen = (gint32)strtol(f[3], NULL, 10);
            t->title = opt(f, n, 4);
            t->artist = opt(f, n, 5);
            t->album = opt(f, n, 6);
            if (n > 7 && f[7][0]) t->playcount = (guint32)strtoul(f[7], NULL, 10);
            t->filetype = g_strdup("MPEG audio file");
            t->mediatype = ITDB_MEDIATYPE_AUDIO;
            itdb_track_add(db, t, -1);
            itdb_playlist_add_track(mpl, t, -1);
        } else if ((!strcmp(f[0], "P") || !strcmp(f[0], "S")) && n >= 2) {
            Itdb_Playlist *pl = itdb_playlist_new(f[1], f[0][0] == 'S');
            itdb_playlist_add(db, pl, -1);
            if (n >= 3 && f[2][0]) members(db, pl, f[2]);
        } else {
            fprintf(stderr, "bad manifest line: %s\n", f[0]);
            return 2;
        }
    }
    fclose(in);

    GError *err = NULL;
    if (!itdb_write_file(db, argv[1], &err)) {
        fprintf(stderr, "write failed: %s\n", err ? err->message : "?");
        return 1;
    }
    itdb_free(db);
    return 0;
}
