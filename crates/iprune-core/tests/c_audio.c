/* Helper for the replace tests. Uses only libgpod, on a fake iPod in a temp dir.
 * set <mp> <dbid hex>:  gives the track the per-file audio data of an older encoding
 *                       (gapless data, start and stop times, Sound Check, bookmark,
 *                       samplerate 22050) and plays, rating and dates of its own,
 *                       then writes with itdb_write.
 * show <mp> <dbid hex>: prints those fields as name=value lines. */
#include <gpod/itdb.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static Itdb_Track *find(Itdb_iTunesDB *db, guint64 dbid) {
    for (GList *l = db->tracks; l; l = l->next)
        if (((Itdb_Track *)l->data)->dbid == dbid) return l->data;
    fprintf(stderr, "no track %llx\n", (unsigned long long)dbid);
    return NULL;
}

static Itdb_iTunesDB *parse(const char *mp) {
    GError *err = NULL;
    Itdb_iTunesDB *db = itdb_parse(mp, &err);
    if (!db) fprintf(stderr, "parse: %s\n", err ? err->message : "?");
    return db;
}

static int set(const char *mp, guint64 dbid) {
    Itdb_iTunesDB *db = parse(mp);
    if (!db) return 1;
    Itdb_Track *t = find(db, dbid);
    if (!t) return 1;
    t->pregap = 576;
    t->postgap = 1100;
    t->samplecount = 8820000;
    t->gapless_data = 12345;
    t->gapless_track_flag = 1;
    t->gapless_album_flag = 1;
    t->soundcheck = 1200;
    t->starttime = 5000;
    t->stoptime = 190000;
    t->bookmark_time = 7000;
    t->samplerate = 22050;
    t->samplerate2 = 22050.0f;
    t->playcount = 7;
    t->skipcount = 2;
    t->rating = 80;
    t->time_added = 1600000000;
    t->time_played = 1650000000;
    GError *err = NULL;
    if (!itdb_write(db, &err)) { fprintf(stderr, "write: %s\n", err ? err->message : "?"); return 1; }
    itdb_free(db);
    return 0;
}

static int show(const char *mp, guint64 dbid) {
    Itdb_iTunesDB *db = parse(mp);
    if (!db) return 1;
    Itdb_Track *t = find(db, dbid);
    if (!t) return 1;
    printf("pregap=%u\npostgap=%u\nsamplecount=%llu\ngapless_data=%u\ngapless_track_flag=%u\n"
           "gapless_album_flag=%u\nsoundcheck=%u\nstarttime=%u\nstoptime=%u\nbookmark_time=%u\n"
           "samplerate=%u\nsamplerate2=%.1f\nplaycount=%u\nskipcount=%u\nrating=%u\ntime_added=%lld\n"
           "time_played=%lld\n",
           t->pregap, t->postgap, (unsigned long long)t->samplecount, t->gapless_data,
           t->gapless_track_flag, t->gapless_album_flag, t->soundcheck, t->starttime, t->stoptime,
           t->bookmark_time, t->samplerate, t->samplerate2, t->playcount, t->skipcount, t->rating,
           (long long)t->time_added, (long long)t->time_played);
    itdb_free(db);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 4 && !strcmp(argv[1], "set")) return set(argv[2], strtoull(argv[3], NULL, 16));
    if (argc == 4 && !strcmp(argv[1], "show")) return show(argv[2], strtoull(argv[3], NULL, 16));
    fprintf(stderr, "usage: set|show <mountpoint> <dbid hex>\n");
    return 2;
}
