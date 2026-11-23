package net.windowstream.android

import android.media.MediaCodec
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build

/**
 * VP9 capability diagnostics via MediaCodec.
 *
 * Design decision (see README): the Android codec adapter must use
 * MediaCodec hardware VP9 decode, not libvpx, because hardware decode
 * is the whole power argument on the client. This object answers, on
 * the actual device:
 *  - is a VP9 decoder present at all,
 *  - is any VP9 decoder hardware-accelerated / software-only,
 *  - what are its size limits,
 *  - can a decoder instance actually be created.
 * It also lists VP9 encoders (relevant for the Android host path).
 */
object CodecDiagnostics {

    const val VP9_MIME: String = MediaFormat.MIMETYPE_VIDEO_VP9

    data class CodecEntry(
        val name: String,
        val isEncoder: Boolean,
        val isHardware: Boolean?,
        val isSoftwareOnly: Boolean?,
        val maxWidth: Int?,
        val maxHeight: Int?,
    )

    fun collectReport(): String {
        val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
        val decoders = mutableListOf<CodecEntry>()
        val encoders = mutableListOf<CodecEntry>()

        for (info in list.codecInfos) {
            if (!info.supportedTypes.any { it.equals(VP9_MIME, ignoreCase = true) }) continue
            val caps = try {
                info.getCapabilitiesForType(VP9_MIME)
            } catch (t: Throwable) {
                null
            }
            val videoCaps = caps?.videoCapabilities
            val entry = CodecEntry(
                name = info.name,
                isEncoder = info.isEncoder,
                isHardware = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) info.isHardwareAccelerated else null,
                isSoftwareOnly = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) info.isSoftwareOnly else null,
                maxWidth = videoCaps?.supportedWidths?.upper,
                maxHeight = videoCaps?.supportedHeights?.upper,
            )
            if (info.isEncoder) encoders += entry else decoders += entry
        }

        val canCreate = try {
            val codec = MediaCodec.createDecoderByType(VP9_MIME)
            val createdName = codec.name
            codec.release()
            "yes ($createdName)"
        } catch (t: Throwable) {
            "no (${t.javaClass.simpleName}: ${t.message})"
        }

        val hasHwDecoder = decoders.any { it.isHardware == true }
        val sb = StringBuilder()
        sb.appendLine("Device: ${Build.MANUFACTURER} ${Build.MODEL} (API ${Build.VERSION.SDK_INT})")
        sb.appendLine("VP9 MIME: $VP9_MIME")
        sb.appendLine("VP9 decoder present: ${decoders.isNotEmpty()}")
        sb.appendLine("VP9 hardware decoder present: $hasHwDecoder")
        sb.appendLine("VP9 decoder instantiable: $canCreate")
        sb.appendLine()
        sb.appendLine("Decoders (${decoders.size}):")
        if (decoders.isEmpty()) sb.appendLine("  (none)")
        for (e in decoders) sb.appendLine("  - ${describe(e)}")
        sb.appendLine()
        sb.appendLine("Encoders (${encoders.size}):")
        if (encoders.isEmpty()) sb.appendLine("  (none)")
        for (e in encoders) sb.appendLine("  - ${describe(e)}")
        sb.appendLine()
        sb.appendLine("Verdict: " + when {
            decoders.isEmpty() -> "NO VP9 DECODE - this device cannot be a window-stream client in v1 (VP9 is fixed, no negotiation)."
            hasHwDecoder -> "OK - hardware VP9 decode available via MediaCodec."
            else -> "SOFTWARE ONLY - VP9 decode exists but is not hardware-accelerated; expect the decoder power cost the design tries to avoid."
        })
        return sb.toString().trimEnd()
    }

    private fun describe(e: CodecEntry): String {
        val hw = when {
            e.isHardware == true -> "hardware"
            e.isSoftwareOnly == true -> "software"
            e.isHardware == false -> "non-hardware"
            else -> "hw-unknown (pre-API 29 reporting)"
        }
        val size = if (e.maxWidth != null && e.maxHeight != null) " max ${e.maxWidth}x${e.maxHeight}" else ""
        return "${e.name} [$hw]$size"
    }

    /** Kept for callers that only need the boolean summary. */
    fun hasHardwareVp9Decoder(): Boolean {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return false
        val list = MediaCodecList(MediaCodecList.REGULAR_CODECS)
        return list.codecInfos.any { info ->
            !info.isEncoder &&
                info.supportedTypes.any { it.equals(VP9_MIME, ignoreCase = true) } &&
                info.isHardwareAccelerated
        }
    }
}
