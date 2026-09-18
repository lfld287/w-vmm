//! Typed errors at the public backend and VM boundaries.
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Platform(#[from] PlatformError),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[error(transparent)]
    Boot(#[from] BootError),
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Serial(#[from] SerialError),
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("disk is read-only")]
    ReadOnly,
    #[error("qcow2 capacity mismatch: expected {expected}, got {actual}")]
    CapacityMismatch { expected: u64, actual: u64 },
    #[error("disk must be a regular file")]
    NotRegularFile,
    #[error("disk capacity must be a positive multiple of 512 (got {size})")]
    InvalidCapacity { size: u64 },
    #[error("invalid header length {length}")]
    InvalidHeaderLength { length: u32 },
    #[error(
        "qcow2 incompatible features (dirty/corrupt/external/compressed/extended) unsupported; run qemu-img check"
    )]
    IncompatibleFeatures { features: u64 },
    #[error("internal snapshots unsupported")]
    InternalSnapshotsUnsupported,
    #[error("encrypted images unsupported")]
    EncryptedImagesUnsupported,
    #[error("invalid qcow2 cluster size {bits}")]
    InvalidQcow2ClusterSize { bits: u32 },
    #[error("backing chains unsupported")]
    BackingChainsUnsupported,
    #[error("only qcow2 v2/v3 supported (got {version})")]
    UnsupportedVersion { version: u32 },
    #[error("invalid/truncated qcow2 header")]
    InvalidHeader,
    #[error("disk request out of bounds: offset {offset}, length {len}, capacity {size}")]
    OutOfBounds { size: u64, offset: u64, len: usize },
    /// Error supplied by a caller-provided backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{operation} {path:?}: {source}")]
    Operation {
        operation: &'static str,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("VM has stopped")]
    Stopped,
    #[error(
        "requested memory {requested} MiB must be a multiple of 128 MiB within region capacity {capacity} MiB"
    )]
    InvalidTarget { requested: u64, capacity: u64 },
    #[error("memory capacity overflow: {mib} MiB")]
    CapacityOverflow { mib: u64 },
    #[error("readable descriptor after writable descriptor")]
    ReadableDescriptorAfterWritableDescriptor,
    #[error("invalid virtio-mem request buffers: readable {read}, writable {write}")]
    InvalidVirtioMemRequestBuffers { read: usize, write: usize },
    #[error("IPA overflow")]
    IpaOverflow,
    #[error("hotplug overflow")]
    HotplugOverflow,
    #[error("hotplug alignment: address {address:#x}")]
    HotplugAlignment { address: u64 },
    #[error("virtio-mem region must be a positive multiple of 128 MiB (got {mib})")]
    InvalidRegionSize { mib: u64 },
    #[error("memory mapping rollback failed; stopping VM: {source}; rollback: {rollback}")]
    RollbackFailed {
        #[source]
        source: Box<MemoryError>,
        rollback: Box<MemoryError>,
    },
    /// Error supplied by a caller-provided backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    GuestMemory(#[from] vm_memory::mmap::FromRangesError),
    #[error(transparent)]
    GuestAccess(#[from] vm_memory::GuestMemoryError),
    #[error(transparent)]
    Slice(#[from] std::array::TryFromSliceError),
    #[error("memory mapping: {0}")]
    Mapping(#[source] Box<PlatformError>),
    #[error(transparent)]
    Regions(#[from] vm_memory::GuestRegionCollectionError),
}

#[derive(Debug, Error)]
pub enum NetError {
    #[error("invalid vmnet RX frame size {length}, capacity {capacity}")]
    InvalidVmnetRxFrameSize { length: usize, capacity: usize },
    #[error("invalid vmnet RX count {count}")]
    InvalidVmnetRxCount { count: i32 },
    #[error("vmnet RX buffer too small: {length}, need {capacity}")]
    VmnetRxBufferTooSmall { length: usize, capacity: usize },
    #[error("invalid vmnet TX count {count}")]
    InvalidVmnetTxCount { count: i32 },
    #[error("invalid vmnet TX frame size {length}, capacity {capacity}")]
    InvalidVmnetTxFrameSize { length: usize, capacity: usize },
    #[error("create vmnet description")]
    CreateVmnetDescription,
    #[error("invalid vmnet frame capacity")]
    InvalidVmnetFrameCapacity,
    #[error("vmnet returned unexpected MTU {mtu}")]
    UnexpectedMtu { mtu: u16 },
    #[error("missing vmnet interface parameters")]
    MissingVmnetInterfaceParameters,
    #[error("invalid vmnet MAC address")]
    InvalidVmnetMacAddress,
    #[error("{operation}: vmnet error {status}")]
    Vmnet { operation: String, status: u32 },
    #[error(
        "{operation}: vmnet permission denied ({status}); run with root privileges or an Apple-approved com.apple.vm.networking entitlement"
    )]
    PermissionDenied { operation: String, status: u32 },
    /// Error supplied by a caller-provided backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    Integer(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error("invalid vmnet MAC address: {0}")]
    MacParse(#[from] std::num::ParseIntError),
    #[error("{operation}: {source}")]
    Wait {
        operation: &'static str,
        #[source]
        source: std::sync::mpsc::RecvTimeoutError,
    },
    #[error(
        "create vmnet shared interface (requires root or an Apple-approved networking entitlement): {0}"
    )]
    Start(#[source] Box<NetError>),
    #[error("missing vmnet MAC")]
    MissingMac,
    #[error("start vmnet returned no interface (check root/networking entitlement)")]
    MissingInterface,
    #[error("vmnet interface is closed")]
    Closed,
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("no default platform; use Vmm::run_with_platform")]
    NoDefaultPlatform,
    #[error("hotplug layout mismatch")]
    HotplugLayoutMismatch,
    #[error(
        "invalid hotplug capacity/alignment: address {address:#x}, size {size}, capacity {capacity}, alignment {alignment}"
    )]
    InvalidHotplugCapacityAlignment {
        address: u64,
        size: u64,
        capacity: u64,
        alignment: u64,
    },
    #[error("invalid virtio MMIO region")]
    InvalidVirtioMmioRegion,
    #[error("RAM capacity mismatch: expected {expected_mib} MiB, got {actual} bytes")]
    RamCapacityMismatch { expected_mib: u64, actual: u64 },
    #[error("RAM capacity overflow: {total} + {size}")]
    RamCapacityOverflow { total: u64, size: u64 },
    #[error("overlapping regions: address {address:#x}, size {size}")]
    OverlappingRegions { address: u64, size: u64 },
    #[error("port range overflow: address {address:#x}, size {size}")]
    PortRangeOverflow { address: u64, size: u64 },
    #[error("region overflow: address {address:#x}, size {size}")]
    RegionOverflow { address: u64, size: u64 },
    #[error("empty region")]
    EmptyRegion,
    #[error("UART region too small: {size}")]
    UartRegionTooSmall { size: u64 },
    #[error("device count mismatch: expected {expected}, got {actual}")]
    DeviceCountMismatch { expected: usize, actual: usize },
    #[error("no base RAM")]
    NoBaseRam,
    #[error("vCPU count must be positive")]
    InvalidVcpuCount,
    #[error("all vCPU event senders disconnected")]
    EventChannelDisconnected,
    #[error("VM not prepared")]
    VmNotPrepared,
    #[error("hotplug overflow")]
    HotplugOverflow,
    #[error("vCPU count outside HVF supported range")]
    UnsupportedVcpuCount,
    #[error("HVF host-page alignment")]
    HvfHostPageAlignment,
    #[error("GIC overlaps RAM")]
    GicOverlapsRam,
    #[error("create GIC configuration")]
    CreateGicConfiguration,
    #[error("too many devices for host GIC SPI range {base}..{end}")]
    GicSpiRange { base: u32, end: u32 },
    #[error("invalid GIC SPI range")]
    InvalidGicSpiRange,
    #[error("{operation}: Hypervisor.framework error {code:#x}")]
    Hypervisor { operation: String, code: i32 },
    #[error("unexpected HVF exit {reason}, ESR={syndrome:#x}")]
    UnexpectedHvfExit { reason: u32, syndrome: u64 },
    #[error("unhandled HVF exception: ESR={syndrome:#x}, address={address:#x}, PC={pc:#x}")]
    UnhandledException {
        syndrome: u64,
        address: u64,
        pc: u64,
    },
    #[error("unsupported sysreg {sysreg:#x}, ESR={esr:#x}")]
    UnsupportedSysreg { sysreg: u64, esr: u64 },
    #[error("unsupported data abort: ESR={syndrome:#x}, address={address:#x}")]
    UnsupportedDataAbort { syndrome: u64, address: u64 },
    #[error("vCPU initialization thread failed")]
    VcpuInitializationThreadFailed,
    #[error("vCPU thread panicked")]
    VcpuThreadPanicked,
    #[error("startup cancelled")]
    StartupCancelled,
    #[error("VM stopped during memory transaction")]
    VmStoppedDuringMemoryTransaction,
    /// Error supplied by a caller-provided backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync + 'static>),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[error(transparent)]
    Boot(#[from] BootError),
    #[error(transparent)]
    Integer(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[derive(Debug, Error)]
pub enum BootError {
    #[error("loader/layout disagreement")]
    LoaderLayoutDisagreement,
    #[error("FDT exceeds reserved space: {length}")]
    FdtExceedsReservedSpace { length: usize },
    #[error("at least one CPU required")]
    AtLeastOneCpuRequired,
    #[error("too many virtio devices for GIC SPIs: {count}")]
    TooManyVirtioDevicesForGicSpis { count: usize },
    #[error("kernel/initramfs/FDT overlap or exceed RAM")]
    KernelInitramfsFdtOverlapOrExceedRam,
    #[error("Image size overflow")]
    ImageSizeOverflow,
    #[error("initrd too large: {length}")]
    InitrdTooLarge { length: usize },
    #[error("RAM address overflow")]
    RamAddressOverflow,
    #[error("RAM size {size} exceeds host allocation size")]
    RamExceedsHostAllocationSize { size: usize },
    #[error("kernel offset overflow: {offset:#x}")]
    KernelOffsetOverflow { offset: u64 },
    #[error("unaligned kernel text offset {offset:#x}")]
    UnalignedKernelTextOffset { offset: u64 },
    #[error("big endian kernel unsupported")]
    BigEndianKernelUnsupported,
    #[error("legacy Image without image_size is unsupported")]
    LegacyImageWithoutImageSizeIsUnsupported,
    #[error("invalid ARM64 Image")]
    InvalidArm64Image,
    #[error("memory must be at least 128 MiB (got {mib})")]
    MemoryMustBeAtLeast128Mib { mib: u64 },
    #[error("memory range {start:#x}..{end:#x} exceeds HVF {bits}-bit IPA range")]
    IpaRange { start: u64, end: u64, bits: u32 },
    #[error("memory address overflow: start {start:#x}, size {size}")]
    MemoryAddressOverflow { start: u64, size: u64 },
    #[error("invalid HVF IPA width {bits}")]
    InvalidIpaWidth { bits: u32 },
    #[error("memory capacity conversion overflow: {mib} MiB")]
    MemoryCapacityConversionOverflow { mib: u64 },
    #[error(transparent)]
    Integer(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    GuestAccess(#[from] vm_memory::GuestMemoryError),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[error(transparent)]
    Fdt(#[from] vm_fdt::Error),
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[error(transparent)]
    Loader(#[from] linux_loader::loader::Error),
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("invalid GET_ID")]
    InvalidGetId,
    #[error("invalid FLUSH")]
    InvalidFlush,
    #[error("sector overflow: {sector}")]
    SectorOverflow { sector: u64 },
    #[error("write to read-only disk")]
    WriteToReadOnlyDisk,
    #[error("unaligned block request: length {length}")]
    UnalignedBlockRequest { length: usize },
    #[error("block descriptor direction mismatch")]
    BlockDescriptorDirectionMismatch,
    #[error("block request length {length} exceeds limit {limit}")]
    BlockRequestTooLarge { length: usize, limit: usize },
    #[error("request length overflow")]
    RequestLengthOverflow,
    #[error("invalid block header/status")]
    InvalidBlockHeaderStatus,
    #[error("truncated block descriptor chain")]
    TruncatedBlockDescriptorChain,
    #[error("block name must be 1..20 ASCII letters, digits, '.', '_' or '-'")]
    InvalidBlockName { name: String },
    #[error("missing block chain")]
    MissingBlockChain,
    #[error("RX length overflow")]
    RxLengthOverflow,
    #[error("RX descriptor must be writable")]
    RxDescriptorMustBeWritable,
    #[error("missing RX chain")]
    MissingRxChain,
    #[error("invalid backend RX packet size {length}, capacity {capacity}")]
    InvalidBackendRxPacketSize { length: usize, capacity: usize },
    #[error("unnegotiated TX checksum/GSO offload")]
    UnnegotiatedTxChecksumGsoOffload,
    #[error("invalid TX packet size {length}, capacity {capacity}")]
    InvalidTxPacketSize { length: usize, capacity: usize },
    #[error("TX length overflow")]
    TxLengthOverflow,
    #[error("TX descriptor must be readable")]
    TxDescriptorMustBeReadable,
    #[error("network MAC must be nonzero unicast")]
    NetworkMacMustBeNonzeroUnicast,
    #[error("invalid network MTU/frame capacity: {mtu}/{capacity}")]
    InvalidNetworkMtuFrameCapacity { mtu: u16, capacity: usize },
    #[error("invalid queue memory")]
    InvalidQueueMemory,
    #[error("invalid QueueReady: {value}")]
    InvalidQueueReady { value: u32 },
    #[error("queue too large: {size}, maximum {max}")]
    QueueTooLarge { size: u32, max: u16 },
    #[error("queue configuration while ready")]
    QueueConfigurationWhileReady,
    #[error("queue configuration after DRIVER_OK")]
    QueueConfigurationAfterDriverOk,
    #[error("virtio DRIVER_OK before valid negotiation/queues")]
    DriverNotReady,
    #[error("virtio status bits cannot be cleared without reset")]
    StatusBitsCleared,
    #[error("cyclic descriptor chain")]
    CyclicDescriptorChain,
    #[error(
        "invalid descriptor flags/range: address {address:#x}, length {length}, flags {flags:#x}"
    )]
    InvalidDescriptorFlagsRange {
        address: u64,
        length: u32,
        flags: u16,
    },
    #[error("unnegotiated indirect descriptor")]
    UnnegotiatedIndirectDescriptor,
    #[error("descriptor index out of range: {index}, queue size {size}")]
    DescriptorIndexOutOfRange { index: u16, size: u16 },
    #[error("invalid available descriptor")]
    InvalidAvailableDescriptor,
    #[error("available ring overrun: {count} entries, queue size {size}")]
    AvailableRingOverrun { count: u16, size: u16 },
    #[error("invalid virtqueue")]
    InvalidVirtqueue,
    #[error("cyclic pending descriptor chain")]
    CyclicPendingDescriptorChain,
    #[error("invalid pending descriptor: address {address:#x}, length {length}, flags {flags:#x}")]
    InvalidPendingDescriptor {
        address: u64,
        length: u32,
        flags: u16,
    },
    #[error("invalid live queue")]
    InvalidLiveQueue,
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    GuestAccess(#[from] vm_memory::GuestMemoryError),
    #[error(transparent)]
    Queue(#[from] virtio_queue::Error),
    #[error(transparent)]
    Slice(#[from] std::array::TryFromSliceError),
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("VM was cancelled before run")]
    Cancelled,
    #[error("blocking VM control on the VMM thread")]
    OwnerThread,
    #[error("VM control disconnected")]
    Disconnected,
    #[error("VM is not running")]
    NotRunning,
    #[error("virtio-mem is not configured")]
    MemoryNotConfigured,
    #[error("{message}")]
    CleanupFailed { message: String },
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error("{0}")]
    ReplyFailed(String),
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("VMM thread panicked")]
    VmmThreadPanicked,
    #[error("unmapped I/O {address:#x}, size {width}")]
    UnmappedIo { address: u64, width: usize },
    #[error("virtio MMIO writes must be 32 bit (got {width} bytes)")]
    InvalidMmioWidth { width: usize },
    #[error("invalid I/O width {width}")]
    InvalidIoWidth { width: usize },
}

#[derive(Debug, Error)]
pub enum SerialError {
    #[error("serial recv returned {count} bytes, exceeding buffer capacity {capacity}")]
    InvalidReceiveLength { count: usize, capacity: usize },
    #[error("serial I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("UART: {0}")]
    Uart(#[from] vm_superio::serial::Error<std::io::Error>),
}

impl From<PlatformError> for MemoryError {
    fn from(source: PlatformError) -> Self {
        Self::Mapping(Box::new(source))
    }
}

/// Render the error and its complete source chain for logs and string protocols.
pub fn diagnostic(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        let cause = error.to_string();
        if !message.ends_with(&cause) {
            message.push_str(": ");
            message.push_str(&cause);
        }
        source = error.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("backend flush")]
    struct BackendFailure(#[source] std::io::Error);

    #[test]
    fn errors_cross_threads_and_keep_backend_causes() {
        fn thread_safe<T: Send + Sync + 'static>() {}
        thread_safe::<Error>();
        let error: Error = StorageError::Backend(Box::new(BackendFailure(std::io::Error::other(
            "host failure",
        ))))
        .into();
        let error = std::thread::spawn(move || error).join().unwrap();
        assert_eq!(diagnostic(&error), "backend flush: host failure");
        assert!(
            matches!(error, Error::Storage(StorageError::Backend(source)) if source.downcast_ref::<BackendFailure>().is_some())
        );
    }
}
