
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