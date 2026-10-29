/*
 * DeckCraft's Media Kit codecs: movies and sounds DeckCraft's own decoders
 * don't read (Motion JPEG, MPEG-1/2, MPEG-4 Part 2, Windows Media, Theora,
 * VP8...), decoded by the system's codecs (the ffmpeg plugin) through
 * BMediaFile and BMediaTrack, from the media's bytes in memory.
 *
 * src/media_kit.rs offers them to the app (deckcraft_media::platform); the
 * bindings are in src/host.rs. Every call is from one Rust thread at a time
 * per handle. The bytes stay owned by Rust and must outlive the handle that
 * reads them (src/host.rs keeps them together).
 */

#include <DataIO.h>
#include <MediaDefs.h>
#include <MediaFile.h>
#include <MediaFormats.h>
#include <MediaTrack.h>

#include <new>
#include <stdlib.h>
#include <string.h>


extern "C" {

// What dc_media_probe found; the strings are NUL-terminated.
struct DcMediaInfo {
	char container[32];
	int64_t duration_us;
	int32_t has_video;
	int32_t video_decodable;
	int32_t width;
	int32_t height;
	float fps;
	int64_t frames;
	char video_codec[96];
	int32_t has_audio;
	int32_t audio_decodable;
	int32_t channels;
	float rate;
	char audio_codec[96];
};

}


namespace {


// A BMediaFile over bytes in memory.
struct Source {
	BMemoryIO io;
	BMediaFile file;

	Source(const void* data, size_t length)
		:
		io(data, length),
		file(&io)
	{
	}
};


Source*
OpenSource(const void* data, size_t length)
{
	if (data == NULL || length == 0)
		return NULL;
	Source* source = new(std::nothrow) Source(data, length);
	if (source != NULL && source->file.InitCheck() != B_OK) {
		delete source;
		return NULL;
	}
	return source;
}


// The first track of the type (B_MEDIA_ENCODED_VIDEO or _AUDIO), or NULL;
// release it with source->file.ReleaseTrack().
BMediaTrack*
FirstTrack(Source* source, media_type type, media_format* encoded)
{
	int32 count = source->file.CountTracks();
	for (int32 i = 0; i < count; i++) {
		BMediaTrack* track = source->file.TrackAt(i);
		if (track == NULL)
			continue;
		if (track->EncodedFormat(encoded) == B_OK && encoded->type == type)
			return track;
		source->file.ReleaseTrack(track);
	}
	return NULL;
}


// Asks the track's decoder for B_RGB32 frames of the track's size.
status_t
NegotiateVideo(BMediaTrack* track, const media_format& encoded,
	media_format* decoded)
{
	uint32 width = encoded.u.encoded_video.output.display.line_width;
	uint32 height = encoded.u.encoded_video.output.display.line_count;
	decoded->Clear();
	decoded->type = B_MEDIA_RAW_VIDEO;
	decoded->u.raw_video = media_raw_video_format::wildcard;
	decoded->u.raw_video.display.format = B_RGB32;
	decoded->u.raw_video.display.line_width = width;
	decoded->u.raw_video.display.line_count = height;
	decoded->u.raw_video.display.bytes_per_row = width * 4;
	status_t status = track->DecodedFormat(decoded);
	if (status != B_OK)
		return status;
	const media_video_display_info& display = decoded->u.raw_video.display;
	if (display.format != B_RGB32 || display.line_width == 0
		|| display.line_count == 0 || display.line_width > 16384
		|| display.line_count > 16384
		|| display.bytes_per_row < display.line_width * 4)
		return B_MEDIA_BAD_FORMAT;
	return B_OK;
}


// Asks the track's decoder for interleaved float samples.
status_t
NegotiateAudio(BMediaTrack* track, media_format* decoded)
{
	decoded->Clear();
	decoded->type = B_MEDIA_RAW_AUDIO;
	decoded->u.raw_audio = media_raw_audio_format::wildcard;
	decoded->u.raw_audio.format = media_raw_audio_format::B_AUDIO_FLOAT;
	decoded->u.raw_audio.byte_order = B_MEDIA_HOST_ENDIAN;
	status_t status = track->DecodedFormat(decoded);
	if (status != B_OK)
		return status;
	const media_raw_audio_format& raw = decoded->u.raw_audio;
	if (raw.channel_count < 1 || raw.channel_count > 32
		|| !(raw.frame_rate >= 1000 && raw.frame_rate <= 768000)
		|| raw.byte_order != B_MEDIA_HOST_ENDIAN)
		return B_MEDIA_BAD_FORMAT;
	switch (raw.format) {
		case media_raw_audio_format::B_AUDIO_FLOAT:
		case media_raw_audio_format::B_AUDIO_INT:
		case media_raw_audio_format::B_AUDIO_SHORT:
		case media_raw_audio_format::B_AUDIO_UCHAR:
		case media_raw_audio_format::B_AUDIO_CHAR:
			return B_OK;
		default:
			return B_MEDIA_BAD_FORMAT;
	}
}


void
CodecName(BMediaTrack* track, char* name, size_t size)
{
	media_codec_info info;
	if (track->GetCodecInfo(&info) == B_OK)
		strlcpy(name, info.pretty_name[0] != '\0' ? info.pretty_name
			: info.short_name, size);
}


// A video track being played: the decoder, and a buffer for its frames.
struct Video {
	Source* source;
	BMediaTrack* track;
	uint8* buffer;
	size_t bufferSize;
	uint32 width;
	uint32 height;
	uint32 bytesPerRow;
};


}	// namespace


extern "C" {


// Describes the media in data. Returns 0, or -1 when the Media Kit can't
// read it.
int32_t
dc_media_probe(const void* data, size_t length, DcMediaInfo* info)
{
	memset(info, 0, sizeof(*info));
	Source* source = OpenSource(data, length);
	if (source == NULL)
		return -1;
	media_file_format fileFormat;
	if (source->file.GetFileFormatInfo(&fileFormat) == B_OK)
		strlcpy(info->container, fileFormat.short_name, sizeof(info->container));

	media_format encoded;
	BMediaTrack* track = FirstTrack(source, B_MEDIA_ENCODED_VIDEO, &encoded);
	if (track != NULL) {
		info->has_video = 1;
		info->width = encoded.u.encoded_video.output.display.line_width;
		info->height = encoded.u.encoded_video.output.display.line_count;
		info->fps = encoded.u.encoded_video.output.field_rate;
		info->frames = track->CountFrames();
		info->duration_us = track->Duration();
		CodecName(track, info->video_codec, sizeof(info->video_codec));
		media_format decoded;
		info->video_decodable
			= NegotiateVideo(track, encoded, &decoded) == B_OK;
		source->file.ReleaseTrack(track);
	}
	track = FirstTrack(source, B_MEDIA_ENCODED_AUDIO, &encoded);
	if (track != NULL) {
		info->has_audio = 1;
		info->channels = encoded.u.encoded_audio.output.channel_count;
		info->rate = encoded.u.encoded_audio.output.frame_rate;
		if (track->Duration() > info->duration_us)
			info->duration_us = track->Duration();
		CodecName(track, info->audio_codec, sizeof(info->audio_codec));
		media_format decoded;
		if (NegotiateAudio(track, &decoded) == B_OK) {
			info->audio_decodable = 1;
			info->channels = decoded.u.raw_audio.channel_count;
			info->rate = decoded.u.raw_audio.frame_rate;
		}
		source->file.ReleaseTrack(track);
	}
	delete source;
	return 0;
}


// Opens the video track of data for decoding to B_RGB32 frames of
// *width × *height, and names the container (32 bytes). NULL when there's
// none the Media Kit decodes.
void*
dc_video_open(const void* data, size_t length, int32_t* width,
	int32_t* height, char* container)
{
	*width = 0;
	*height = 0;
	container[0] = '\0';
	Source* source = OpenSource(data, length);
	if (source == NULL)
		return NULL;
	media_file_format fileFormat;
	if (source->file.GetFileFormatInfo(&fileFormat) == B_OK)
		strlcpy(container, fileFormat.short_name, 32);
	media_format encoded;
	media_format decoded;
	BMediaTrack* track = FirstTrack(source, B_MEDIA_ENCODED_VIDEO, &encoded);
	if (track == NULL || NegotiateVideo(track, encoded, &decoded) != B_OK) {
		if (track != NULL)
			source->file.ReleaseTrack(track);
		delete source;
		return NULL;
	}
	const media_video_display_info& display = decoded.u.raw_video.display;
	Video* video = new(std::nothrow) Video;
	size_t bufferSize = (size_t)display.bytes_per_row * display.line_count;
	uint8* buffer = (uint8*)malloc(bufferSize);
	if (video == NULL || buffer == NULL) {
		delete video;
		free(buffer);
		source->file.ReleaseTrack(track);
		delete source;
		return NULL;
	}
	video->source = source;
	video->track = track;
	video->buffer = buffer;
	video->bufferSize = bufferSize;
	video->width = display.line_width;
	video->height = display.line_count;
	video->bytesPerRow = display.bytes_per_row;
	*width = video->width;
	*height = video->height;
	return video;
}


// Decodes the next frame into rgba (width × height × 4 bytes, opaque RGBA)
// and its presentation time. Returns 1, 0 at the end of the track, or -1
// on an error.
int32_t
dc_video_next(void* handle, uint8_t* rgba, size_t length, int64_t* time)
{
	Video* video = (Video*)handle;
	if (length < (size_t)video->width * video->height * 4)
		return -1;
	int64 count = 1;
	media_header header;
	memset(&header, 0, sizeof(header));
	status_t status = video->track->ReadFrames(video->buffer, &count, &header);
	if (status == B_LAST_BUFFER_ERROR || (status == B_OK && count == 0))
		return 0;
	if (status != B_OK)
		return -1;
	*time = header.start_time;
	for (uint32 y = 0; y < video->height; y++) {
		const uint8* in = video->buffer + (size_t)y * video->bytesPerRow;
		uint8* out = rgba + (size_t)y * video->width * 4;
		for (uint32 x = 0; x < video->width; x++) {
			// B_RGB32 is B, G, R, unused in memory.
			out[0] = in[2];
			out[1] = in[1];
			out[2] = in[0];
			out[3] = 255;
			in += 4;
			out += 4;
		}
	}
	return 1;
}


// Moves to the key frame at or before time (microseconds); returns the
// time decoding resumes at.
int64_t
dc_video_seek(void* handle, int64_t time)
{
	Video* video = (Video*)handle;
	bigtime_t at = time;
	if (video->track->SeekToTime(&at, B_MEDIA_SEEK_CLOSEST_BACKWARD) != B_OK) {
		at = 0;
		video->track->SeekToTime(&at, B_MEDIA_SEEK_CLOSEST_BACKWARD);
	}
	return at;
}


void
dc_video_close(void* handle)
{
	Video* video = (Video*)handle;
	if (video == NULL)
		return;
	video->source->file.ReleaseTrack(video->track);
	delete video->source;
	free(video->buffer);
	delete video;
}


// Decodes the first audio track of data to interleaved float samples
// (malloc'd, *count of them, at most maxSamples). NULL when there's none the
// Media Kit decodes.
float*
dc_audio_decode(const void* data, size_t length, size_t maxSamples,
	int32_t* channels, float* rate, size_t* count)
{
	*channels = 0;
	*rate = 0;
	*count = 0;
	Source* source = OpenSource(data, length);
	if (source == NULL)
		return NULL;
	media_format encoded;
	media_format decoded;
	BMediaTrack* track = FirstTrack(source, B_MEDIA_ENCODED_AUDIO, &encoded);
	if (track == NULL || NegotiateAudio(track, &decoded) != B_OK) {
		if (track != NULL)
			source->file.ReleaseTrack(track);
		delete source;
		return NULL;
	}
	const media_raw_audio_format raw = decoded.u.raw_audio;
	size_t sampleSize = raw.format & media_raw_audio_format::B_AUDIO_SIZE_MASK;
	size_t frameSize = sampleSize * raw.channel_count;
	// ReadFrames fills up to buffer_size bytes; leave room for decoders that
	// return more than they said.
	size_t bufferSize = raw.buffer_size > 0 ? raw.buffer_size * 4 : 1 << 20;
	if (bufferSize < 1 << 16)
		bufferSize = 1 << 16;
	uint8* buffer = (uint8*)malloc(bufferSize);
	size_t capacity = 1 << 20;
	float* samples = (float*)malloc(capacity * sizeof(float));
	size_t total = 0;
	bool failed = buffer == NULL || samples == NULL;
	while (!failed) {
		int64 frames = 0;
		media_header header;
		status_t status = track->ReadFrames(buffer, &frames, &header);
		if (status != B_OK || frames <= 0)
			break;
		size_t n = (size_t)frames * raw.channel_count;
		if ((size_t)frames * frameSize > bufferSize)
			break;
		if (total + n > maxSamples)
			break;
		if (total + n > capacity) {
			size_t grown = capacity * 2;
			while (grown < total + n)
				grown *= 2;
			float* more = (float*)realloc(samples, grown * sizeof(float));
			if (more == NULL) {
				failed = true;
				break;
			}
			samples = more;
			capacity = grown;
		}
		float* out = samples + total;
		switch (raw.format) {
			case media_raw_audio_format::B_AUDIO_FLOAT:
				memcpy(out, buffer, n * sizeof(float));
				break;
			case media_raw_audio_format::B_AUDIO_INT:
				for (size_t i = 0; i < n; i++)
					out[i] = ((const int32*)buffer)[i] / 2147483648.0f;
				break;
			case media_raw_audio_format::B_AUDIO_SHORT:
				for (size_t i = 0; i < n; i++)
					out[i] = ((const int16*)buffer)[i] / 32768.0f;
				break;
			case media_raw_audio_format::B_AUDIO_UCHAR:
				for (size_t i = 0; i < n; i++)
					out[i] = (buffer[i] - 128) / 128.0f;
				break;
			default:
				for (size_t i = 0; i < n; i++)
					out[i] = ((const int8*)buffer)[i] / 128.0f;
				break;
		}
		total += n;
	}
	free(buffer);
	source->file.ReleaseTrack(track);
	delete source;
	if (failed || total == 0) {
		free(samples);
		return NULL;
	}
	*channels = raw.channel_count;
	*rate = raw.frame_rate;
	*count = total;
	return samples;
}


}	// extern "C"
