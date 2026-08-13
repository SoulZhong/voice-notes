// issue #98:sherpa-onnx v1.13.4 的 C API(c-api.cc)对离线识别全链路无 try/catch,
// onnxruntime 的 C++ 异常(Ort::Exception 等)会穿过 extern "C" 边界进入 Rust;
// Rust 规定外来异常撞上 catch_unwind 即 abort(__rust_foreign_exception)——三例
// SIGABRT 同签名。本文件是我们自己的异常屏障:把整段识别调用包进 try/catch,
// 异常降级为错误码并把 what() 打到 stderr(顺带补上此前一直缺失的诊断消息)。
//
// 刻意不 include sherpa 头文件:预编译静态库包(sherpa-onnx-prebuilt)的 include
// 路径不在本包构建视野内;这些 C 符号的 ABI 只涉及不透明指针与基本类型,自声明
// 原型即可,链接期由既有的 sherpa 静态库解析。

#include <cstdint>
#include <cstdio>
#include <exception>
#include <stdexcept>

extern "C" {
// sherpa-onnx C API 原型(不透明指针化;与 c-api.h 的 ABI 一致)。
const void *SherpaOnnxCreateOfflineRecognizer(const void *config);
const void *SherpaOnnxCreateOfflineStream(const void *recognizer);
void SherpaOnnxAcceptWaveformOffline(const void *stream, int32_t sample_rate,
                                     const float *samples, int32_t n);
void SherpaOnnxDecodeOfflineStream(const void *recognizer, const void *stream);
const char *SherpaOnnxGetOfflineStreamResultAsJson(const void *stream);
void SherpaOnnxDestroyOfflineStream(const void *stream);
}

namespace {
void log_caught(const char *where, const char *what) {
  // stderr 与 Rust eprintln 同流;下一次真实触发时这行就是根因线索。
  std::fprintf(stderr, "[sherpa-barrier] C++ 异常被捕获于 %s: %s\n", where,
               what ? what : "(unknown)");
  std::fflush(stderr);
}
}  // namespace

extern "C" {

/// 创建识别器的屏障版:异常 → 返回 null(调用方已有 null 检查路径)。
const void *vn_sherpa_create_offline_recognizer(const void *config) {
  try {
    return SherpaOnnxCreateOfflineRecognizer(config);
  } catch (const std::exception &e) {
    log_caught("CreateOfflineRecognizer", e.what());
    return nullptr;
  } catch (...) {
    log_caught("CreateOfflineRecognizer", "non-std exception");
    return nullptr;
  }
}

/// 整段识别(建流→喂波形→解码→取 JSON→销毁流)的屏障版。
/// 返回 0 且 *out_json 为结果指针(调用方用 SherpaOnnxDestroyOfflineStreamResultJson
/// 释放;可能为 null=引擎无结果);返回 -1 表示 C++ 异常已捕获(流已在本侧清理,
/// *out_json 置 null)。
int32_t vn_sherpa_transcribe(const void *recognizer, int32_t sample_rate,
                             const float *samples, int32_t n,
                             const char **out_json) {
  *out_json = nullptr;
  const void *stream = nullptr;
  try {
    stream = SherpaOnnxCreateOfflineStream(recognizer);
    if (stream == nullptr) {
      // 与异常同待遇:上层报"创建识别流失败"。
      return -2;
    }
    SherpaOnnxAcceptWaveformOffline(stream, sample_rate, samples, n);
    SherpaOnnxDecodeOfflineStream(recognizer, stream);
    *out_json = SherpaOnnxGetOfflineStreamResultAsJson(stream);
    SherpaOnnxDestroyOfflineStream(stream);
    return 0;
  } catch (const std::exception &e) {
    log_caught("transcribe", e.what());
  } catch (...) {
    log_caught("transcribe", "non-std exception");
  }
  // 异常路径的流清理也可能再抛(极端),同样不许穿出。
  if (stream != nullptr) {
    try {
      SherpaOnnxDestroyOfflineStream(stream);
    } catch (...) {
      log_caught("transcribe/cleanup", "destroy stream threw");
    }
  }
  return -1;
}

/// 屏障机制自测(单测用,不触碰 sherpa):mode=1 人为 throw 验证 try/catch 与
/// 链接闭环,mode=0 正常返回。若异常未被捕获,测试进程会 SIGABRT 而非断言失败。
int32_t vn_sherpa_barrier_selftest(int32_t mode) {
  try {
    if (mode == 1) {
      throw std::runtime_error("barrier selftest");
    }
    return 0;
  } catch (const std::exception &) {
    return -1;
  }
}

}  // extern "C"
