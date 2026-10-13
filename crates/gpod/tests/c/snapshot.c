/* snapshot <iPod mountpoint or iTunesDB file>: print every track (by permanent dbid) and every
   playlist's members, in a stable form for diffing before/after a write */
#include <gpod/itdb.h>
#include <stdio.h>
int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: snapshot <mountpoint or iTunesDB>\n"); return 2; }
    GError *err = NULL;
    Itdb_iTunesDB *db = g_file_test(argv[1], G_FILE_TEST_IS_DIR) ? itdb_parse(argv[1], &err) : itdb_parse_file(argv[1], &err);
    if (!db) { fprintf(stderr, "parse failed: %s\n", err ? err->message : "?"); return 1; }
    printf("tracks %u playlists %u\n", g_list_length(db->tracks), g_list_length(db->playlists));
    for (GList *l = db->tracks; l; l = l->next) {
        Itdb_Track *t = l->data;
        printf("T %016llx %s | %s | %s | %s | %d | art %d mhii %u | rating %u plays %u\n",
               (unsigned long long)t->dbid, t->ipod_path, t->artist, t->album, t->title,
               t->size, itdb_track_has_thumbnails(t), t->mhii_link, t->rating, t->playcount);
    }
    for (GList *l = db->playlists; l; l = l->next) {
        Itdb_Playlist *pl = l->data;
        printf("P %s%s%s %u\n", pl->name, itdb_playlist_is_mpl(pl) ? " [master]" : "",
               pl->is_spl ? " [smart]" : "", g_list_length(pl->members));
        if (itdb_playlist_is_mpl(pl) || pl->is_spl) continue;
        for (GList *m = pl->members; m; m = m->next)
            printf("  %016llx\n", (unsigned long long)((Itdb_Track *)m->data)->dbid);
    }
    return 0;
}
