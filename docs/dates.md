# How a capture date is derived

From the bytes on disk to the name on the output file.

The archive's layout is a wall clock — `2024/07/15/1430-22417.jpg` — so the only
question a date source has to answer is *what digits go in the path*. Everything
below is in service of that, and [`metadata::taken::Taken`](../src/metadata/taken.rs)
is where the answer is carried: `local` are the digits, `certain` says whether
they are the reading the photographer saw, and `offset` records the zone when a
source stated one. Only `local` reaches the path.

```mermaid
flowchart TD

subgraph SCAN["1 · Scan — util::scan_fs, fs.rs"]
  WALK["FileSystem::walk + metadata()<br/>OS directory or zip entry"]
  SI["ScanInfo<br/>modified: Option i64, epoch ms<br/>created: Option i64, epoch ms"]
  WALK --> SI
end

subgraph PARSE["2 · Inspect — inspect::analyze_file → metadata::media_file_info_from_readable"]
  FT["determine_file_type + metadata_type<br/>content sniff, never the extension"]
  EXIF["exif::parse_exif_info<br/>tags map, incl. SubSecTime*"]
  TRACK["track::parse_track_info<br/>mvhd CreateDate + moov/meta keys"]
  XMP["xmp::parse_xmp — .xmp sidecar<br/>photoshop:DateCreated → xmp:CreateDate → exif:DateTimeOriginal"]
  SUPP["supplemental — Takeout .json<br/>photoTakenTime / creationTime, unix seconds"]
  FT -->|image| EXIF
  FT -->|video| TRACK
end
SI --> FT

subgraph KINDS["3 · Each source says what it knew — metadata::taken::Taken"]
  W["Taken::wall<br/>certain, no offset<br/>digits known, zone unrecorded"]
  Z["Taken::zoned<br/>certain, offset known"]
  IN["Taken::instant<br/>NOT certain<br/>UTC digits standing in for a wall clock"]
end

EXIF -->|"DateTimeOriginal / ModifyDate + SubSec*"| W
EXIF -->|"tag carried an OffsetTime"| Z
EXIF -->|"GPSDateStamp — midnight, UTC by definition"| IN
TRACK -->|"com.apple.quicktime.creationdate"| Z
TRACK -->|"mvhd — spec says UTC, cameras write local"| W
XMP -->|"Taken::parse of the stored string"| W
XMP -->|"stored string carried an offset"| Z
SUPP -->|timestamp_as_utc| IN
SI -->|"util::timestamp_to_utc"| IN

subgraph RANK["4 · reconcile::best_guess_taken_ranked — first hit wins"]
  direction LR
  R1["human<br/>1 XMP datetime<br/>2 photoTakenTime"]
  R2["camera — embedded_reading<br/>3 EXIF: DateTimeOriginal → ModifyDate → GPSDateStamp<br/>4 track: Apple creationdate → mvhd"]
  R3["fallback<br/>5 creationTime (upload)<br/>6 file modified<br/>7 file created"]
  R1 --> R2 --> R3
end
W --> RANK
Z --> RANK
IN --> RANK

subgraph REPAIR["5 · reconcile::best_guess_taken — prefer digits over instants"]
  CERT{"winner.certain?"}
  SAME{"plausible_offset<br/>same second-of-minute,<br/>apart by no more than -12:00..+14:00"}
  KEEPF["keep the winner's digits,<br/>borrow a fraction if it has none"]
  SWAP["take the embedded reading whole,<br/>and record the offset that falls out"]
  KEEP["winner unchanged —<br/>files under its UTC digits"]
  CERT -->|yes| SAME
  CERT -->|no| SAME
  SAME -->|"yes, winner certain"| KEEPF
  SAME -->|"yes, winner uncertain"| SWAP
  SAME -->|no| KEEP
end
RANK --> CERT

subgraph OUT["6 · File it — output_path::get_desired_media_path"]
  LOCAL["reads Taken::local only<br/>no parse, no round trip"]
  DATED["yyyy/mm/dd/hhmm-ssmmm"]
  UND["undated/short_checksum<br/>(only when no source had a date at all)"]
  LOCAL -->|Some| DATED
  LOCAL -->|None| UND
end
KEEPF --> LOCAL
SWAP --> LOCAL
KEEP --> LOCAL

subgraph WRITE["7 · Claim a name and write"]
  DED["dedup::resolve_output_path<br/>bare → -short → -long checksum"]
  CLIP["sync_cmd::derived_for_clip<br/>live-photo clip never computes a date:<br/>it inherits the still's resolved path"]
  FINAL["2024/07/15/1430-22417.jpg"]
  MD["markdown::get_desired_markdown_path<br/>same stem, .md"]
  DED --> FINAL --> MD
  CLIP --> DED
end
DATED --> DED
UND --> DED
DATED --> CLIP

KEEPF -.->|to_rfc3339| FM["note frontmatter: datetime"]
KEEPF -.->|"to_rfc3339 + offset"| DB["db: guessed_datetime,<br/>guessed_utc_offset_s"]
```

## Notes

- **The path never goes through a string.** `get_desired_media_path` takes the
  `Taken` and formats `local`. `best_guess_taken_dt` still produces RFC 3339, but
  only for the note and the index.
- **`guessed_utc_offset_s` is how the index stays honest.** `guessed_datetime`
  writes `+00:00` on a reading whose zone nobody recorded; a NULL offset beside
  it is what says that suffix is a placeholder rather than a reading.
- **`undated/` now means exactly one thing** — no source had a date. It used to
  also catch a date that failed to survive being parsed back out of a string.
- **What is still unrepairable:** a file whose only date is its mtime, and a
  Takeout export whose embedded date was stripped. Both file under UTC digits,
  which is what an `--assume-timezone` option would be for.
