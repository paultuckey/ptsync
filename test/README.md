
# Test files

### Regenerate the live_photo pair

A Live Photo is a still and a short video sharing an Apple content identifier — in the
still's `MakerNote` and in the video's QuickTime metadata. The two are deliberately given
unrelated names, since ptsync pairs them on the identifier and never on the name.

The maker note has to be copied from a real iPhone file; exiftool cannot synthesise one.
Any iPhone photo will do, and the identifier is overwritten afterwards so no real UUID
ends up in the repository.

```shell
cp Canon_40D.jpg live_photo/still.jpg
exiftool -overwrite_original -tagsfromfile SOME_IPHONE_PHOTO.HEIC \
  "-makernotes" "-exif:make" "-exif:model" live_photo/still.jpg
exiftool -overwrite_original \
  "-Apple:ContentIdentifier=11111111-2222-3333-4444-555555555555" live_photo/still.jpg
```

```shell
cp Hello.mov live_photo/clip.mov
exiftool -overwrite_original \
  "-Keys:ContentIdentifier=11111111-2222-3333-4444-555555555555" live_photo/clip.mov
```

`Hello.mov` carries no usable capture time, which is what makes it a good half of the
pair: filed on its own it lands under 1904, so a test asserting the still's date proves
the video really did take the still's name.

### Regenerate Hello.webp

A small WebP that carries a real EXIF block, copied from `Canon_40D.jpg`. The EXIF is
the point of the fixture: `nom-exif` cannot read a WebP's `EXIF` RIFF chunk, so this
file is what pins `metadata_type` to `NoMetadata` — if that support ever lands upstream,
this fixture will start yielding a 2008 capture date and the test will say so.

```shell
cwebp -metadata exif -q 60 -resize 160 120 Canon_40D.jpg -o Hello.webp
```

### Regenerate the still-image fixtures

`Hello.tif` and `Hello.avif` keep the EXIF copied across from `Canon_40D.jpg`, because
the point of both is that the capture clock survives — `nom-exif` reads TIFF and AVIF,
which is also why raw is routed to the EXIF reader. `Hello.bmp` has no EXIF to keep;
BMP has nowhere to put it.

```shell
magick Canon_40D.jpg -resize 80x60 -compress LZW Hello.tif
exiftool -overwrite_original -tagsfromfile Canon_40D.jpg -all:all Hello.tif
magick Canon_40D.jpg -resize 80x60 Hello.bmp
magick Canon_40D.jpg -resize 160x120 Hello.avif
exiftool -overwrite_original -tagsfromfile Canon_40D.jpg -all:all Hello.avif
```

### Regenerate the video fixtures

One-second solid-colour clips, the same recipe as `Hello.wmv` below.

```shell
ffmpeg -y -f lavfi -i color=c=blue:s=160x120:r=10:d=1 -c:v libx264 -b:v 60k -an Hello.3gp
ffmpeg -y -f lavfi -i color=c=blue:s=160x120:r=10:d=1 -c:v libx264 -b:v 60k -an -f mpegts Hello.mts
ffmpeg -y -f lavfi -i color=c=blue:s=160x120:r=10:d=1 -c:v libvpx-vp9 -b:v 40k -an Hello.webm
ffmpeg -y -f lavfi -i color=c=blue:s=160x120:r=10:d=1 -c:v libx264 -b:v 60k -an -f matroska Hello.mkv
```

### Regenerate Hello_pnot.mov

A QuickTime file that opens with a `pnot` preview atom instead of a brand, the way
2000s Nikon compacts wrote them. `file-format` 0.29 does not sniff that shape, so
these are dropped as arbitrary binary — the fixture is what pins the workaround in
`is_pnot_quicktime`, and `test_pnot_workaround_still_needed` fails once
[the upstream fix](https://github.com/mmalecot/file-format/pull/90) is released and
both can go.

ffmpeg cannot write a preview atom, so the fixture is built from an ordinary clip:
drop `ftyp` and `wide`, prepend `pnot` and a small `PICT`, and shift every `stco`
chunk offset by the number of bytes inserted. Skipping that last step leaves a file
that sniffs correctly but no longer plays, which would make the fixture prove nothing.

```shell
ffmpeg -y -f lavfi -i color=c=blue:s=160x120:r=10:d=1 -c:v libx264 -b:v 60k -an base.mov
python3 pnot_fixture.py base.mov Hello_pnot.mov
ffmpeg -v error -i Hello_pnot.mov -f null -       # must still decode cleanly
```

### Regenerate Hello.mka

Matroska carrying audio instead of video — the case that must *not* be treated as media,
and the reason the `reader-ebml` feature is enabled: without it every EBML file looks
alike and this one would be filed as a video with nothing to show. The Matroska
equivalent of `Hello.wma`.

```shell
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=0.5" -c:a aac -b:a 24k -f matroska Hello.mka
```

### Regenerate Hello.wmv

A one-second solid-colour clip in an ASF container. It needs a real video stream:
sniffing walks the ASF header to tell a WMV apart from a `.wma` audio track, so an
audio-only or empty file would be classified as unsupported instead.

```shell
ffmpeg -y -f lavfi -i color=c=blue:s=160x120:r=10:d=1 -c:v wmv2 -b:v 60k -an Hello.wmv
```

### Regenerate Hello.wma

The same ASF container carrying audio instead of video — the case that must *not*
be treated as media. 24 kbps is the encoder's floor.

```shell
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=0.5" -c:a wmav2 -b:a 24k -ar 8000 -ac 1 Hello.wma
```

### Add modified time to mp4

```shell
exiftool -m -P -overwrite_original -AllDates="2024:04:18 11:24:26" Hello.mp4
```

### Show all exif fields for Canon_40D.jpg

```shell
exiftool -a -G1 -s Canon_40D.jpg
```