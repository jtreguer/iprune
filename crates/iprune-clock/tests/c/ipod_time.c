/* libgpod's tools/ipod-scsi.c, sync_time: the struct and the fill logic,
 * with plain C types and htons in place of glib's. Prints the 12 bytes of
 * the local time (the TZ environment variable) of the epoch second given,
 * as 24 hex digits. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

struct iPodTime {
    uint16_t year;
    uint16_t days;
    unsigned char timezone;
    unsigned char hour;
    unsigned char minute;
    unsigned char second;
    unsigned char dst;
    unsigned char padding[3];
} __attribute__((__packed__));

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    time_t t = (time_t)strtoll(argv[1], NULL, 10);
    struct tm tm_buf;
    struct tm *tm;
    struct iPodTime ipod_time;
    tzset();
    tm = localtime_r(&t, &tm_buf);
    if (tm == NULL) return 1;

    ipod_time.year = htons(1900 + tm->tm_year);
    ipod_time.days = htons(tm->tm_yday);
    ipod_time.timezone = tm->tm_gmtoff / 900;
    ipod_time.hour = tm->tm_hour;
    ipod_time.minute = tm->tm_min;
    ipod_time.second = tm->tm_sec;
    ipod_time.dst = tm->tm_isdst ? 1 : 0;
    memset(ipod_time.padding, 0, 3);

    if (sizeof ipod_time != 12) return 3;
    const unsigned char *p = (const unsigned char *)&ipod_time;
    for (int i = 0; i < 12; i++) printf("%02x", p[i]);
    printf("\n");
    return 0;
}
