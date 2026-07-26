
# Test files



### Add modified time to mp4

```shell
exiftool -m -P -overwrite_original -AllDates="2024:04:18 11:24:26" Hello.mp4
```

### Show all exif fields for Canon_40D.jpg

```shell
exiftool -a -G1 -s Canon_40D.jpg
```

### Generate Hello.wmv

Needs both streams: ASF only resolves to WMV rather than WMA when the header
declares a video stream. `wmav2` rejects bitrates below 24k.

```shell
ffmpeg -f lavfi -i "color=c=darkblue:size=160x120:rate=10:duration=1" -f lavfi -i "anullsrc=channel_layout=mono:sample_rate=22050:duration=1" -t 1 -c:v wmv2 -b:v 16k -c:a wmav2 -b:a 24k Hello.wmv
```