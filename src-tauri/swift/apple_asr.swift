// Apple SpeechTranscriber(macOS 26+)的 C ABI 桥:供 Rust 侧 asr::apple 调用。
//
// 只做三件事:报状态、装语言包、识别一段 16kHz 单声道 f32。全部同步阻塞——调用方
// 是 Rust 的识别线程(不在 Swift 并发池里),用信号量等 async 结果是安全的。
//
// 三个已踩过的坑(2026-10-05 原型实测):
// 1. 用前必须 AssetInventory.reserve(locale:),否则框架在 preRunRecognition 里直接
//    trap(EXC_BREAKPOINT),整个进程跟着崩——Rust 侧没有机会兜底。
// 2. 流式输入必须是 bestAvailableAudioFormat(实测 16k Int16),直接喂 Float32 同样 trap。
// 3. 语言包没装好时同样会 trap,所以识别前先查 installedLocales,不在就报错返回。
//
// 部署目标是 macOS 13(与应用一致),新 API 全部在 #available 后面,旧系统上
// vn_apple_asr_status 返回 0,其余入口返回错误,不会碰到新符号。

import AVFoundation
import Foundation
import Speech

private let localeId = "zh-CN"

/// 0 = 系统不支持(< macOS 26);1 = 不支持中文;2 = 中文语言包未装;3 = 可用。
@_cdecl("vn_apple_asr_status")
public func vn_apple_asr_status() -> Int32 {
    guard #available(macOS 26, *) else { return 0 }
    return blocking { await AppleAsr.shared.status() }
}

/// 下载并安装中文语言包(阻塞直到完成)。成功返回 0;失败返回非 0,err_out 里是错误文本
/// (调用方用 vn_apple_asr_free 释放)。
@_cdecl("vn_apple_asr_install")
public func vn_apple_asr_install(_ errOut: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32 {
    guard #available(macOS 26, *) else {
        errOut.pointee = strdup("需要 macOS 26 或更新版本")
        return 1
    }
    let err: String? = blocking {
        do {
            try await AppleAsr.shared.install()
            return nil
        } catch {
            return "\(error)"
        }
    }
    if let err {
        errOut.pointee = strdup(err)
        return 1
    }
    return 0
}

/// 识别一段音频。成功返回 0,out 为 JSON {"text","tokens","timestamps"};失败返回非 0,
/// out 为错误文本。两种情况 out 都要用 vn_apple_asr_free 释放。
@_cdecl("vn_apple_asr_recognize")
public func vn_apple_asr_recognize(
    _ samples: UnsafePointer<Float>?,
    _ count: Int64,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> Int32 {
    guard #available(macOS 26, *) else {
        out.pointee = strdup("需要 macOS 26 或更新版本")
        return 1
    }
    let pcm: [Float] = (samples != nil && count > 0)
        ? Array(UnsafeBufferPointer(start: samples, count: Int(count)))
        : []
    let result: Result<String, Error> = blocking {
        do {
            return .success(try await AppleAsr.shared.recognize(pcm))
        } catch {
            return .failure(error)
        }
    }
    switch result {
    case .success(let json):
        out.pointee = strdup(json)
        return 0
    case .failure(let error):
        out.pointee = strdup("\(error)")
        return 1
    }
}

@_cdecl("vn_apple_asr_free")
public func vn_apple_asr_free(_ p: UnsafeMutablePointer<CChar>?) {
    free(p)
}

/// 在非 Swift 并发线程上同步等一个 async 闭包的结果。
private func blocking<T>(_ body: @escaping @Sendable () async -> T) -> T {
    let sem = DispatchSemaphore(value: 0)
    let box = Box<T>()
    Task.detached {
        box.value = await body()
        sem.signal()
    }
    sem.wait()
    return box.value!
}

private final class Box<T>: @unchecked Sendable {
    var value: T?
}

private struct AsrError: Error, CustomStringConvertible {
    let description: String
}

@available(macOS 26, *)
private actor AppleAsr {
    static let shared = AppleAsr()

    private var reserved = false
    private var bestFormat: AVAudioFormat?

    private func locale() async -> Locale? {
        await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: localeId))
    }

    private func makeTranscriber(_ locale: Locale) -> SpeechTranscriber {
        SpeechTranscriber(
            locale: locale,
            transcriptionOptions: [],
            reportingOptions: [],
            attributeOptions: [.audioTimeRange]
        )
    }

    private func isInstalled(_ locale: Locale) async -> Bool {
        let installed = await SpeechTranscriber.installedLocales
        return installed.contains { $0.identifier(.bcp47) == locale.identifier(.bcp47) }
    }

    func status() async -> Int32 {
        guard let locale = await locale() else { return 1 }
        return await isInstalled(locale) ? 3 : 2
    }

    func install() async throws {
        guard let locale = await locale() else {
            throw AsrError(description: "系统语音识别不支持中文")
        }
        if let req = try await AssetInventory.assetInstallationRequest(supporting: [makeTranscriber(locale)]) {
            try await req.downloadAndInstall()
        }
        guard await isInstalled(locale) else {
            throw AsrError(description: "中文语言包安装后仍不可用")
        }
    }

    func recognize(_ pcm: [Float]) async throws -> String {
        guard let locale = await locale() else {
            throw AsrError(description: "系统语音识别不支持中文")
        }
        guard await isInstalled(locale) else {
            throw AsrError(description: "中文语言包未安装")
        }
        if !reserved {
            // 返回 false 表示已预留过(或名额已满但本 locale 已在其中),两种都可以继续;
            // 名额满且不含本 locale 时下方 analyzer 会 trap,所以这里核一遍。
            _ = try await AssetInventory.reserve(locale: locale)
            let ids = await AssetInventory.reservedLocales.map { $0.identifier(.bcp47) }
            guard ids.contains(locale.identifier(.bcp47)) else {
                throw AsrError(description: "系统语言包预留名额已满,无法预留中文")
            }
            reserved = true
        }
        if pcm.isEmpty {
            return try encode(text: "", tokens: [], timestamps: [])
        }

        let transcriber = makeTranscriber(locale)
        if bestFormat == nil {
            bestFormat = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber])
        }
        guard let target = bestFormat else {
            throw AsrError(description: "取不到识别器的音频格式")
        }
        let buffer = try convert(pcm, to: target)

        let analyzer = SpeechAnalyzer(modules: [transcriber])
        let (stream, cont) = AsyncStream<AnalyzerInput>.makeStream()
        let collect = Task { () throws -> (String, [String], [Float]) in
            var text = ""
            var tokens: [String] = []
            var timestamps: [Float] = []
            for try await r in transcriber.results {
                text += String(r.text.characters)
                // 每个 run 都收,标点/空格这类不带时间范围的 run 沿用上一个时间戳:
                // 下游按 tokens 拼接还原文本(diar::split),丢 run 就会丢字。
                for run in r.text.runs {
                    let piece = String(r.text[run.range].characters)
                    if piece.isEmpty { continue }
                    let ts = run.audioTimeRange.map { Float($0.start.seconds) } ?? (timestamps.last ?? 0)
                    tokens.append(piece)
                    timestamps.append(ts)
                }
            }
            return (text, tokens, timestamps)
        }
        try await analyzer.start(inputSequence: stream)
        cont.yield(AnalyzerInput(buffer: buffer))
        cont.finish()
        try await analyzer.finalizeAndFinishThroughEndOfInput()
        let (text, tokens, timestamps) = try await collect.value
        return try encode(text: text, tokens: tokens, timestamps: timestamps)
    }

    private func convert(_ pcm: [Float], to target: AVAudioFormat) throws -> AVAudioPCMBuffer {
        guard let src = AVAudioFormat(standardFormatWithSampleRate: 16000, channels: 1),
              let input = AVAudioPCMBuffer(pcmFormat: src, frameCapacity: AVAudioFrameCount(pcm.count))
        else { throw AsrError(description: "无法创建输入缓冲") }
        input.frameLength = AVAudioFrameCount(pcm.count)
        pcm.withUnsafeBufferPointer { input.floatChannelData![0].update(from: $0.baseAddress!, count: pcm.count) }
        if target == src { return input }

        guard let conv = AVAudioConverter(from: src, to: target) else {
            throw AsrError(description: "无法创建格式转换器")
        }
        let ratio = target.sampleRate / src.sampleRate
        let cap = AVAudioFrameCount((Double(pcm.count) * ratio).rounded(.up)) + 1024
        guard let output = AVAudioPCMBuffer(pcmFormat: target, frameCapacity: cap) else {
            throw AsrError(description: "无法创建输出缓冲")
        }
        var fed = false
        var err: NSError?
        let status = conv.convert(to: output, error: &err) { _, st in
            if fed {
                st.pointee = .endOfStream
                return nil
            }
            fed = true
            st.pointee = .haveData
            return input
        }
        if status == .error {
            throw AsrError(description: "音频格式转换失败: \(err?.localizedDescription ?? "未知")")
        }
        return output
    }

    private func encode(text: String, tokens: [String], timestamps: [Float]) throws -> String {
        let obj: [String: Any] = ["text": text, "tokens": tokens, "timestamps": timestamps]
        let data = try JSONSerialization.data(withJSONObject: obj)
        return String(decoding: data, as: UTF8.self)
    }
}
