/* make_fixture <out iTunesDB>: write a small database full of edge cases for
   the snapshot comparison test. Uses only libgpod, so the fixture does not
   depend on the Rust code under test. */
#include <gpod/itdb.h>
#include <stdio.h>

static Itdb_Track *track(Itdb_iTunesDB *db, guint64 dbid, const char *path, const char *artist,
                         const char *album, const char *title, guint32 size, guint32 rating,
                         guint32 plays) {
    Itdb_Track *t = itdb_track_new();
    t->dbid = dbid;
    t->ipod_path = g_strdup(path);
    t->artist = g_strdup(artist);
    t->album = g_strdup(album);
    t->title = g_strdup(title);
    t->filetype = g_strdup("MPEG audio file");
    t->size = size;
    t->tracklen = 200000;
    t->rating = rating;
    t->playcount = plays;
    t->mediatype = ITDB_MEDIATYPE_AUDIO;
    itdb_track_add(db, t, -1);
    itdb_playlist_add_track(itdb_playlist_mpl(db), t, -1);
    return t;
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: make_fixture <out>\n"); return 2; }
    Itdb_iTunesDB *db = itdb_new();
    Itdb_Playlist *mpl = itdb_playlist_new("iPod de Test", FALSE);
    itdb_playlist_set_mpl(mpl);
    itdb_playlist_add(db, mpl, -1);

    Itdb_Track *a = track(db, 0x86536232fbbc81cfULL, ":iPod_Control:Music:F00:AAAA.mp3",
                          "Björk", "Homogenic", "Jóga", 4123456, 80, 12);
    Itdb_Track *b = track(db, 0x0000000000000001ULL, ":iPod_Control:Music:F01:BBBB.m4a",
                          NULL, NULL, "No artist or album", 1, 0, 0);
    Itdb_Track *c = track(db, 0xffffffffffffffffULL, ":iPod_Control:Music:F49:CCCC.wav",
                          "", "", "", 3000000000u, 100, 4294967295u);
    Itdb_Track *d = track(db, 0x00ab00cd00ef0012ULL, ":iPod_Control:Music:F12:DDDD.mp3",
                          "Sigur Rós | pipe", "Ágætis byrjun", "日本語 タイトル 🎵", 0, 20, 3);

    Itdb_Playlist *p1 = itdb_playlist_new("80s", FALSE);
    itdb_playlist_add(db, p1, -1);
    itdb_playlist_add_track(p1, d, -1);
    itdb_playlist_add_track(p1, a, -1);
    itdb_playlist_add_track(p1, d, -1); /* repeated member */
    itdb_playlist_add_track(p1, c, -1);

    itdb_playlist_add(db, itdb_playlist_new("+Empty é", FALSE), -1);

    Itdb_Playlist *smart = itdb_playlist_new("Top rated", TRUE);
    itdb_playlist_add(db, smart, -1);
    itdb_playlist_add_track(smart, a, -1);

    Itdb_Playlist *p2 = itdb_playlist_new("-Last", FALSE);
    itdb_playlist_add(db, p2, -1);
    itdb_playlist_add_track(p2, b, -1);

    GError *err = NULL;
    if (!itdb_write_file(db, argv[1], &err)) {
        fprintf(stderr, "write failed: %s\n", err ? err->message : "?");
        return 1;
    }
    itdb_free(db);
    return 0;
}
