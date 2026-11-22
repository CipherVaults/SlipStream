# Third-party notices

The SlipStream Windows installer and portable ZIP include the following third-party executables in `bin/`.

## yt-dlp

The bundled `yt-dlp.exe` is the upstream Windows x64 standalone build of **yt-dlp 2026.10.26**. The yt-dlp project source is available under The Unlicense, included as `bin/yt-dlp-LICENSE.txt`. The standalone executable combines yt-dlp with separately licensed components and is distributed under GPLv3 or later. [Upstream release](https://github.com/yt-dlp/yt-dlp/releases/tag/2026.10.26), [The Unlicense](https://github.com/yt-dlp/yt-dlp/blob/2026.10.26/LICENSE), [bundled component licenses](https://github.com/yt-dlp/yt-dlp/blob/2026.10.26/THIRD_PARTY_LICENSES.txt)

The matching upstream source tarball `yt-dlp.tar.gz` is published alongside the SlipStream installer and ZIPs as `yt-dlp-2026.10.26-source.tar.gz`. The pinned upstream build workflow and component notices identify the standalone build and its dependencies. [Upstream source tarball](https://github.com/yt-dlp/yt-dlp/releases/download/2026.10.26/yt-dlp.tar.gz), [standalone build workflow](https://github.com/yt-dlp/yt-dlp/blob/2026.10.26/.github/workflows/build.yml), [bundled component licenses](https://github.com/yt-dlp/yt-dlp/blob/2026.10.26/THIRD_PARTY_LICENSES.txt)

## FFmpeg and ffprobe

`ffmpeg.exe` and `ffprobe.exe` come from the **GyanD/codexffmpeg 9.1 Essentials** Windows x64 build (`ffmpeg-9.1-essentials_build.zip`), distributed under GPLv3. The installer and portable ZIP include `bin/ffmpeg-LICENSE.txt` and `bin/ffmpeg-README.txt` from the build archive. Gyan identifies FFmpeg commit `74e4393595` as the source for this build. [GyanD/codexffmpeg 9.1 release](https://github.com/GyanD/codexffmpeg/releases/tag/9.1), [FFmpeg source commit](https://github.com/FFmpeg/FFmpeg/commit/74e4393595), [FFmpeg source at that commit](https://github.com/FFmpeg/FFmpeg/tree/74e4393595), [GyanD 9.1 release repository tarball](https://github.com/GyanD/codexffmpeg/archive/refs/tags/9.1.tar.gz), [libraries used in the builds](https://www.gyan.dev/ffmpeg/builds/#libraries)

When redistributing the GPL-covered executables, retain the applicable notices and license texts and provide access to the corresponding source as required by GPLv3. Corresponding source includes the applicable bundled components and build scripts. [GNU GPLv3](https://www.gnu.org/licenses/gpl-3.0.html), [GNU GPL FAQ on providing source](https://www.gnu.org/licenses/gpl-faq.html#AnonFTPAndSendSources)
