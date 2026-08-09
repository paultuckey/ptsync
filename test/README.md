
# Test files

`Canon_40D.jpg` comes from
<https://github.com/ianare/exif-samples/blob/master/jpg/Canon_40D.jpg>.

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