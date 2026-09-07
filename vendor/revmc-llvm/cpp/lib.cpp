#include <llvm-c/LLJIT.h>
#include <llvm-c/LLJITUtils.h>
#include <llvm-c/Orc.h>
#include <llvm-c/TargetMachine.h>
#include <llvm/ExecutionEngine/JITLink/JITLink.h>
#include <llvm/ExecutionEngine/Orc/AbsoluteSymbols.h>
#include <llvm/ExecutionEngine/Orc/CompileUtils.h>
#include <llvm/ExecutionEngine/Orc/Debugging/DebugInfoSupport.h>
#include <llvm/ExecutionEngine/Orc/Debugging/DebuggerSupport.h>
#include <llvm/ExecutionEngine/Orc/Debugging/PerfSupportPlugin.h>
#include <llvm/ExecutionEngine/Orc/LLJIT.h>
#include <llvm/ExecutionEngine/Orc/ObjectLinkingLayer.h>
#include <llvm/ExecutionEngine/Orc/TargetProcess/JITLoaderGDB.h>
#include <llvm/ExecutionEngine/Orc/TargetProcess/JITLoaderPerf.h>
#include <llvm/IR/Attributes.h>
#include <llvm/IR/GlobalValue.h>
#include <llvm/Target/TargetMachine.h>

#include <atomic>
#include <fstream>
#include <unistd.h>

using namespace llvm;

extern "C" LLVMAttributeRef revmc_llvm_create_initializes_attr(LLVMContextRef C,
                                                               int64_t Lower,
                                                               int64_t Upper) {
  auto &Ctx = *unwrap(C);
  unsigned BitWidth = 64;
  ConstantRange CR(APInt(BitWidth, Lower, true), APInt(BitWidth, Upper, true));
  return wrap(Attribute::get(Ctx, Attribute::Initializes, ArrayRef(CR)));
}

/// Remove a JITDylib from the ExecutionSession.
extern "C" LLVMErrorRef
revmc_llvm_execution_session_remove_jit_dylib(LLVMOrcExecutionSessionRef ES,
                                              LLVMOrcJITDylibRef JD) {
  auto *Session = reinterpret_cast<orc::ExecutionSession *>(ES);
  auto *Dylib = reinterpret_cast<orc::JITDylib *>(JD);
  return wrap(Session->removeJITDylib(*Dylib));
}

/// Add a JITDylib to the link order of another JITDylib.
extern "C" void
revmc_llvm_jit_dylib_add_to_link_order(LLVMOrcJITDylibRef JD,
                                       LLVMOrcJITDylibRef Other) {
  auto *Dylib = reinterpret_cast<orc::JITDylib *>(JD);
  auto *OtherDylib = reinterpret_cast<orc::JITDylib *>(Other);
  Dylib->addToLinkOrder(*OtherDylib);
}

/// Use ConcurrentIRCompiler (thread-safe, fresh TargetMachine per compilation)
/// while keeping the default InPlaceTaskDispatcher (no background threads).
///
/// Sets the codegen optimization level to Aggressive (O3) for best JIT code
/// quality. The IR optimization passes are run separately with their own level.
///
/// setSupportConcurrentCompilation(true) would also switch the dispatcher to
/// DynamicThreadPoolTaskDispatcher, spawning background threads. We only want
/// the thread-safe compiler, not the thread pool.
extern "C" void revmc_llvm_lljit_builder_set_concurrent_compiler(
    LLVMOrcLLJITBuilderRef Builder) {
  auto *B = reinterpret_cast<orc::LLJITBuilder *>(Builder);
  B->setCompileFunctionCreator(
      [](orc::JITTargetMachineBuilder JTMB)
          -> Expected<std::unique_ptr<orc::IRCompileLayer::IRCompiler>> {
        JTMB.setCodeGenOptLevel(CodeGenOptLevel::Aggressive);
        return std::make_unique<orc::ConcurrentIRCompiler>(std::move(JTMB));
      });
}

/// Synchronous symbol lookup in a specific JITDylib.
/// `Name` is unmangled — LLJIT applies the data layout prefix internally.
extern "C" LLVMErrorRef
revmc_llvm_lljit_lookup_in(LLVMOrcLLJITRef J, LLVMOrcJITDylibRef JD,
                           LLVMOrcExecutorAddress *Result, const char *Name) {
  auto *Jit = reinterpret_cast<orc::LLJIT *>(J);
  auto *Dylib = reinterpret_cast<orc::JITDylib *>(JD);
  auto Addr = Jit->lookup(*Dylib, Name);
  if (!Addr)
    return wrap(Addr.takeError());
  *Result = Addr->getValue();
  return LLVMErrorSuccess;
}

/// JITLink plugin that tracks live code and data bytes.
///
/// Installs a post-allocation pass on each link graph that sums block sizes,
/// split by executable (code) vs non-executable (data) sections. On resource
/// removal the corresponding sizes are subtracted, so the counters reflect
/// current live memory rather than cumulative allocations.
class MemoryUsagePlugin : public orc::ObjectLinkingLayer::Plugin {
  std::atomic<size_t> *CodeBytes, *DataBytes;
  std::mutex Mutex;
  DenseMap<orc::ResourceKey, std::pair<size_t, size_t>> Allocs;

public:
  MemoryUsagePlugin(std::atomic<size_t> *CodeBytes,
                    std::atomic<size_t> *DataBytes)
      : CodeBytes(CodeBytes), DataBytes(DataBytes) {}

  void modifyPassConfig(orc::MaterializationResponsibility &MR,
                        jitlink::LinkGraph &,
                        jitlink::PassConfiguration &Config) override {
    Config.PostAllocationPasses.push_back([this, &MR](
                                              jitlink::LinkGraph &G) -> Error {
      size_t code = 0, data = 0;
      for (auto &section : G.sections()) {
        size_t sec_size = 0;
        for (auto *block : section.blocks())
          sec_size += block->getSize();
        if ((section.getMemProt() & orc::MemProt::Exec) != orc::MemProt::None)
          code += sec_size;
        else
          data += sec_size;
      }
      CodeBytes->fetch_add(code, std::memory_order_relaxed);
      DataBytes->fetch_add(data, std::memory_order_relaxed);
      return MR.withResourceKeyDo([&](orc::ResourceKey Key) {
        std::lock_guard<std::mutex> Lock(Mutex);
        auto &Entry = Allocs[Key];
        Entry.first += code;
        Entry.second += data;
      });
    });
  }

  Error notifyFailed(orc::MaterializationResponsibility &) override {
    return Error::success();
  }

  Error notifyRemovingResources(orc::JITDylib &,
                                orc::ResourceKey Key) override {
    std::lock_guard<std::mutex> Lock(Mutex);
    auto It = Allocs.find(Key);
    if (It != Allocs.end()) {
      CodeBytes->fetch_sub(It->second.first, std::memory_order_relaxed);
      DataBytes->fetch_sub(It->second.second, std::memory_order_relaxed);
      Allocs.erase(It);
    }
    return Error::success();
  }

  void notifyTransferringResources(orc::JITDylib &, orc::ResourceKey DstKey,
                                   orc::ResourceKey SrcKey) override {
    std::lock_guard<std::mutex> Lock(Mutex);
    auto It = Allocs.find(SrcKey);
    if (It != Allocs.end()) {
      auto &Dst = Allocs[DstKey];
      Dst.first += It->second.first;
      Dst.second += It->second.second;
      Allocs.erase(It);
    }
  }
};

/// Install MemoryUsagePlugin on the LLJIT's ObjectLinkingLayer.
///
/// `CodeBytes` and `DataBytes` must be valid for the lifetime of the LLJIT.
/// Returns an error if the object layer is not JITLink-based.
extern "C" LLVMErrorRef
revmc_llvm_lljit_enable_memory_usage(LLVMOrcLLJITRef J,
                                     std::atomic<size_t> *CodeBytes,
                                     std::atomic<size_t> *DataBytes) {
  auto *Jit = reinterpret_cast<orc::LLJIT *>(J);
  auto *OLL = dyn_cast<orc::ObjectLinkingLayer>(&Jit->getObjLinkingLayer());
  if (!OLL)
    return wrap(make_error<StringError>("MemoryUsagePlugin requires JITLink",
                                        inconvertibleErrorCode()));
  OLL->addPlugin(std::make_unique<MemoryUsagePlugin>(CodeBytes, DataBytes));
  return LLVMErrorSuccess;
}

/// JITLink plugin that writes `/tmp/perf-<pid>.map` in the perf map format.
///
/// For each finalized link graph the plugin emits one line per named,
/// executable symbol:
///
///     <hex_addr> <hex_size> <name>\n
///
/// This is the format expected by `perf report --jit` and `samply` to resolve
/// JIT-compiled symbols without the heavyweight jitdump machinery.
///
/// NOTE: Not suitable for long-running programs. The map file is append-only
/// and never cleaned up, so entries for freed JIT code accumulate indefinitely.
/// Prefer `PerfSupportPlugin` (jitdump) for long-lived processes.
class SimplePerfSupportPlugin : public orc::ObjectLinkingLayer::Plugin {
  std::mutex Mutex;
  std::ofstream MapFile;

public:
  SimplePerfSupportPlugin()
      : MapFile("/tmp/perf-" + std::to_string(getpid()) + ".map",
                std::ios::app) {}

  void modifyPassConfig(orc::MaterializationResponsibility &,
                        jitlink::LinkGraph &,
                        jitlink::PassConfiguration &Config) override {
    Config.PostFixupPasses.push_back([this](jitlink::LinkGraph &G) -> Error {
      if (!MapFile)
        return Error::success();
      std::lock_guard<std::mutex> Lock(Mutex);
      for (auto *Sym : G.defined_symbols()) {
        if (!Sym->hasName())
          continue;
        auto &Section = Sym->getBlock().getSection();
        if ((Section.getMemProt() & orc::MemProt::Exec) == orc::MemProt::None)
          continue;
        auto Addr = Sym->getAddress().getValue();
        auto Size = Sym->getSize();
        MapFile << std::hex << Addr << ' ' << Size << ' '
                << (*Sym->getName()).str() << '\n';
      }
      MapFile.flush();
      return Error::success();
    });
  }

  Error notifyFailed(orc::MaterializationResponsibility &) override {
    return Error::success();
  }

  Error notifyRemovingResources(orc::JITDylib &, orc::ResourceKey) override {
    return Error::success();
  }

  void notifyTransferringResources(orc::JITDylib &, orc::ResourceKey,
                                   orc::ResourceKey) override {}
};

/// Install `SimplePerfSupportPlugin` on the LLJIT's `ObjectLinkingLayer`.
///
/// Writes `/tmp/perf-<pid>.map` in the perf map format so that profilers like
/// `perf` and `samply` can resolve JIT-compiled symbols without jitdump.
///
/// Not suitable for long-running programs; see `SimplePerfSupportPlugin`.
///
/// Returns an error if the object layer is not JITLink-based.
extern "C" LLVMErrorRef revmc_llvm_lljit_enable_simple_perf(LLVMOrcLLJITRef J) {
  auto *Jit = reinterpret_cast<orc::LLJIT *>(J);
  auto *OLL = dyn_cast<orc::ObjectLinkingLayer>(&Jit->getObjLinkingLayer());
  if (!OLL)
    return wrap(make_error<StringError>(
        "SimplePerfSupportPlugin requires JITLink", inconvertibleErrorCode()));
  OLL->addPlugin(std::make_unique<SimplePerfSupportPlugin>());
  return LLVMErrorSuccess;
}

/// Install PerfSupportPlugin on the LLJIT's ObjectLinkingLayer.
/// Writes perf jitdump records so `perf record -k 1` / `perf inject --jit`
/// can resolve JIT-compiled symbols with full debug info and unwind info.
///
/// Instead of using `PerfSupportPlugin::Create()` which dynamically looks up
/// JITLoaderPerf symbols (and can crash in certain linker configurations), we
/// manually register the three function addresses as absolute symbols and
/// construct PerfSupportPlugin directly. This matches the approach used by
/// Julia and jank JITs.
///
/// Returns an error if the object layer is not JITLink-based or the target is
/// not ELF.
extern "C" LLVMErrorRef
revmc_llvm_lljit_enable_perf_support(LLVMOrcLLJITRef J) {
  auto *Jit = reinterpret_cast<orc::LLJIT *>(J);
  auto *OLL = dyn_cast<orc::ObjectLinkingLayer>(&Jit->getObjLinkingLayer());
  if (!OLL)
    return wrap(make_error<StringError>("PerfSupportPlugin requires JITLink",
                                        inconvertibleErrorCode()));
  auto &ES = Jit->getExecutionSession();
  auto &EPC = ES.getExecutorProcessControl();

  if (!EPC.getTargetTriple().isOSBinFormatELF())
    return wrap(make_error<StringError>("PerfSupportPlugin requires ELF target",
                                        inconvertibleErrorCode()));

  // Register JITLoaderPerf function addresses as absolute symbols in the main
  // JITDylib to avoid dynamic symbol lookup crashes.
  auto Flags = JITSymbolFlags::Exported | JITSymbolFlags::Callable;
  orc::SymbolMap PerfFns;
  auto StartAddr =
      orc::ExecutorAddr::fromPtr(&llvm_orc_registerJITLoaderPerfStart);
  auto EndAddr = orc::ExecutorAddr::fromPtr(&llvm_orc_registerJITLoaderPerfEnd);
  auto ImplAddr =
      orc::ExecutorAddr::fromPtr(&llvm_orc_registerJITLoaderPerfImpl);
  PerfFns[ES.intern("llvm_orc_registerJITLoaderPerfStart")] = {StartAddr,
                                                               Flags};
  PerfFns[ES.intern("llvm_orc_registerJITLoaderPerfEnd")] = {EndAddr, Flags};
  PerfFns[ES.intern("llvm_orc_registerJITLoaderPerfImpl")] = {ImplAddr, Flags};
  if (auto Err = Jit->getMainJITDylib().define(orc::absoluteSymbols(PerfFns)))
    return wrap(std::move(Err));

  // Preserve DWARF debug info sections through linking for source mapping.
  auto DebugPlugin = orc::DebugInfoPreservationPlugin::Create();
  if (!DebugPlugin)
    return wrap(DebugPlugin.takeError());
  OLL->addPlugin(std::move(*DebugPlugin));

  OLL->addPlugin(std::make_unique<orc::PerfSupportPlugin>(
      EPC, StartAddr, EndAddr, ImplAddr,
      /*EmitDebugInfo=*/true, /*EmitUnwindInfo=*/true));
  return LLVMErrorSuccess;
}

/// Register the JITLoaderGDB symbol and enable debug support.
///
/// `enableDebuggerSupport` (called by `LLVMOrcLLJITEnableDebugSupport`) looks
/// up the symbol in the ProcessSymbolsJITDylib using `ES.intern()` with a
/// hardcoded `_` prefix on MachO. On macOS, static library symbols aren't
/// exported to dlsym, so the process symbol lookup fails.
///
/// We register the symbol as an absolute symbol in the ProcessSymbolsJITDylib
/// (matching the lookup target) using `ES.intern()` with the same mangling that
/// LLVM uses internally, then call `enableDebuggerSupport` directly.
extern "C" LLVMErrorRef
revmc_llvm_lljit_enable_debug_support(LLVMOrcLLJITRef J) {
  auto *Jit = reinterpret_cast<orc::LLJIT *>(J);
  auto &ES = Jit->getExecutionSession();
  const auto &TT = Jit->getTargetTriple();

  // Match the mangling used by GDBJITDebugInfoRegistrationPlugin::Create:
  // hardcoded '_' prefix on MachO, no prefix otherwise.
  auto SymName = TT.isOSBinFormatMachO()
                     ? ES.intern("_llvm_orc_registerJITLoaderGDBAllocAction")
                     : ES.intern("llvm_orc_registerJITLoaderGDBAllocAction");

  // Define in the ProcessSymbolsJITDylib where enableDebuggerSupport looks.
  auto ProcessJD = Jit->getProcessSymbolsJITDylib();
  auto &TargetJD = ProcessJD ? *ProcessJD : Jit->getMainJITDylib();

  auto Flags = JITSymbolFlags::Exported | JITSymbolFlags::Callable;
  orc::SymbolMap GDBFns;
  GDBFns[SymName] = {
      orc::ExecutorAddr::fromPtr(&llvm_orc_registerJITLoaderGDBAllocAction),
      Flags};
  if (auto Err = TargetJD.define(orc::absoluteSymbols(GDBFns)))
    return wrap(std::move(Err));

  return wrap(orc::enableDebuggerSupport(*Jit));
}

extern "C" void
revmc_llvm_target_machine_set_opt_level(LLVMTargetMachineRef TM,
                                        LLVMCodeGenOptLevel Level) {
  auto *Machine = reinterpret_cast<TargetMachine *>(TM);
  CodeGenOptLevel L = CodeGenOptLevel::Default;
  switch (Level) {
  case LLVMCodeGenLevelNone:
    L = CodeGenOptLevel::None;
    break;
  case LLVMCodeGenLevelLess:
    L = CodeGenOptLevel::Less;
    break;
  case LLVMCodeGenLevelDefault:
    L = CodeGenOptLevel::Default;
    break;
  case LLVMCodeGenLevelAggressive:
    L = CodeGenOptLevel::Aggressive;
    break;
  }
  Machine->setOptLevel(L);
}

extern "C" void revmc_llvm_set_dso_local(LLVMValueRef Val, bool Local) {
  unwrap<GlobalValue>(Val)->setDSOLocal(Local);
}
