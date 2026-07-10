# Example database queries

The `ptsync db` command scans an archive or directory and writes a SQLite
database (`db.sqlite` by default) describing your photo and video collection.
This page collects a handful of ready-to-run queries to help you get a feel for
what's in your collection. See [db-schema.md](db-schema.md) for the full schema.

Open the database and run any of the queries below with the `sqlite3`
command-line tool:

```
sqlite3 db.sqlite
```

<!-- The SQL blocks below are executed against a scanned database by the
     `db_example_queries` test, so keep each block a single valid statement. -->

## How big is my collection?

Total number of items and how much disk space they use.

```sql
SELECT COUNT(*)                             AS items,
       SUM(file_size)                       AS total_bytes,
       ROUND(SUM(file_size) / 1073741824.0, 2) AS total_gb
FROM media_item;
```

## How many photos versus videos?

Each item is tagged with its `kind`: `p` for photo, `v` for video.

```sql
SELECT kind,
       COUNT(*) AS items
FROM media_item
GROUP BY kind;
```

## What file types do I have?

Counts and total size for each detected file type, largest first.

```sql
SELECT accurate_file_type                    AS file_type,
       COUNT(*)                              AS items,
       ROUND(SUM(file_size) / 1048576.0, 1)  AS total_mb
FROM media_item
GROUP BY accurate_file_type
ORDER BY items DESC;
```

## How many photos did I take each year?

Uses the best-guess date derived from metadata.

```sql
SELECT strftime('%Y', guessed_datetime) AS year,
       COUNT(*)                         AS items
FROM media_item
WHERE guessed_datetime IS NOT NULL
GROUP BY year
ORDER BY year;
```

## What are my largest files?

The ten biggest items — handy for finding space hogs.

```sql
SELECT media_path,
       ROUND(file_size / 1048576.0, 1) AS size_mb
FROM media_item
ORDER BY file_size DESC
LIMIT 10;
```

## Which cameras took my photos?

Groups items by the camera make and model recorded in the metadata.

```sql
SELECT camera_make,
       camera_model,
       COUNT(*) AS items
FROM media_item
WHERE camera_model IS NOT NULL
GROUP BY camera_make, camera_model
ORDER BY items DESC
LIMIT 10;
```

## How many items have a location?

Counts items that carry GPS coordinates versus those that do not.

```sql
SELECT COUNT(*) FILTER (WHERE latitude IS NOT NULL) AS with_location,
       COUNT(*) FILTER (WHERE latitude IS NULL)     AS without_location
FROM media_item;
```

## What's the orientation mix?

Portrait, landscape or square, based on the recorded dimensions.

```sql
SELECT COALESCE(orientation, 'unknown') AS orientation,
       COUNT(*)                         AS items
FROM media_item
GROUP BY orientation
ORDER BY items DESC;
```

## Who appears in the most photos?

People come from Google supplemental metadata, linked via `media_person`.

```sql
SELECT p.name,
       COUNT(*) AS appears_in
FROM person p
JOIN media_person mp ON mp.person_id = p.person_id
GROUP BY p.person_id
ORDER BY appears_in DESC
LIMIT 10;
```

## How big is each album?

Every album and how many items belong to it (including empty albums).

```sql
SELECT a.title,
       COUNT(af.media_item_id) AS items
FROM album a
LEFT JOIN album_file af ON af.album_id = a.album_id
GROUP BY a.album_id
ORDER BY items DESC;
```

## Do I have duplicate files?

Items that share the same content hash are exact duplicates. `wasted_bytes`
is the space used by every copy of each duplicated file.

```sql
SELECT long_hash,
       COUNT(*)       AS copies,
       SUM(file_size) AS wasted_bytes
FROM media_item
WHERE long_hash IS NOT NULL
GROUP BY long_hash
HAVING COUNT(*) > 1
ORDER BY copies DESC
LIMIT 10;
```
