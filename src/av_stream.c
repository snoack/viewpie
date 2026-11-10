#include <stddef.h>
#include "wrapper.h"

AVCodecParameters *viewpie_stream_codecpar(AVFormatContext *format, int index) {
	if (!format || index < 0 || index >= format->nb_streams)
		return NULL;
	return format->streams[index]->codecpar;
}
