//! Caller-provided, synchronous serial communication on the VMM thread.
use std::io::{self, Write};
use vm_superio::{Serial, Trigger};

/// Host side of the guest UART. No `Send` or `Sync` bound is required.
/// Output uses synchronous `write_all` and `flush`; errors terminate the VM.
pub trait SerialIo: Write {
    /// Nonblocking input for the guest. Return at most `buffer.len()` bytes.
    /// Zero (including EOF), `WouldBlock`, and `Interrupted` mean no input now.
    /// Other errors terminate the VM and run its disk cleanup.
    fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize>;

    /// Request a clean stop independently of input availability or FIFO space.
    fn should_stop(&self) -> bool {
        false
    }
}

impl<T: SerialIo + ?Sized> SerialIo for Box<T> {
    fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        (**self).recv(buffer)
    }

    fn should_stop(&self) -> bool {
        (**self).should_stop()
    }
}

/// Poll once, returning whether the backend requests a stop.
pub(crate) fn poll_input<T: Trigger<E = io::Error>, SI: SerialIo>(
    serial: &mut Serial<T, vm_superio::serial::NoEvents, SI>,
) -> anyhow::Result<bool> {
    if serial.writer_mut().should_stop() {
        return Ok(true);
    }
    let capacity = serial.fifo_capacity();
    if capacity != 0 {
        let mut buffer = vec![0; capacity];
        let count = match serial.writer_mut().recv(&mut buffer) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                0
            }
            Err(error) => return Err(error.into()),
        };
        if count > buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "serial recv returned more bytes than buffer capacity",
            )
            .into());
        }
        if count != 0 {
            serial.enqueue_raw_bytes(&buffer[..count])?;
        }
    }
    Ok(serial.writer_mut().should_stop())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::VecDeque, rc::Rc};

    struct Irq;

    impl Trigger for Irq {
        type E = io::Error;

        fn trigger(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct Memory {
        input: VecDeque<u8>,
        output: Vec<u8>,
        capacities: Vec<usize>,
        stop: Rc<Cell<bool>>,
        read_error: Option<io::ErrorKind>,
        write_error: Option<io::ErrorKind>,
        flush_error: Option<io::ErrorKind>,
        invalid_length: bool,
        stop_on_recv: bool,
    }

    impl Write for Memory {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(kind) = self.write_error {
                return Err(kind.into());
            }
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_error.map_or(Ok(()), |kind| Err(kind.into()))
        }
    }

    impl SerialIo for Memory {
        fn recv(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.capacities.push(buffer.len());
            if let Some(kind) = self.read_error {
                return Err(kind.into());
            }
            if self.invalid_length {
                return Ok(buffer.len() + 1);
            }
            if self.stop_on_recv {
                self.stop.set(true);
            }
            let count = buffer.len().min(self.input.len());
            for byte in &mut buffer[..count] {
                *byte = self.input.pop_front().unwrap();
            }
            Ok(count)
        }

        fn should_stop(&self) -> bool {
            self.stop.get()
        }
    }

    #[test]
    fn bytes_capacity_and_stop_with_full_fifo() {
        let mut serial = Serial::new(Irq, Memory::default());
        let capacity = serial.fifo_capacity();
        let bytes: Vec<_> = (0..capacity + 3).map(|i| i as u8).collect();
        serial.writer_mut().input.extend(&bytes);
        assert!(!poll_input(&mut serial).unwrap());
        assert_eq!(serial.fifo_capacity(), 0);
        assert!(!poll_input(&mut serial).unwrap());
        assert_eq!(serial.writer_mut().capacities, [capacity]);
        assert_eq!(serial.read(0), bytes[0]);
        assert!(!poll_input(&mut serial).unwrap());
        assert_eq!(serial.writer_mut().capacities, [capacity, 1]);
        for byte in &bytes[1..=capacity] {
            assert_eq!(serial.read(0), *byte);
        }
        serial.writer_mut().input.clear();
        serial.writer_mut().input.push_back(0x1d);
        assert!(!poll_input(&mut serial).unwrap());
        assert_eq!(serial.read(0), 0x1d);
        for byte in b"guest output" {
            serial.write(0, *byte).unwrap();
        }
        assert_eq!(serial.writer_mut().output, b"guest output");
        serial.enqueue_raw_bytes(&vec![0; capacity]).unwrap();
        serial.writer_mut().stop.set(true);
        assert!(poll_input(&mut serial).unwrap());
    }

    #[test]
    fn empty_transient_and_fatal_input() {
        let mut serial = Serial::new(Irq, Memory::default());
        for kind in [
            None,
            Some(io::ErrorKind::WouldBlock),
            Some(io::ErrorKind::Interrupted),
        ] {
            serial.writer_mut().read_error = kind;
            assert!(!poll_input(&mut serial).unwrap());
        }
        serial.writer_mut().read_error = Some(io::ErrorKind::BrokenPipe);
        assert_eq!(
            poll_input(&mut serial)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        serial.writer_mut().read_error = None;
        serial.writer_mut().invalid_length = true;
        assert_eq!(
            poll_input(&mut serial)
                .unwrap_err()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn output_errors() {
        for flush in [false, true] {
            let mut backend = Memory::default();
            if flush {
                backend.flush_error = Some(io::ErrorKind::BrokenPipe);
            } else {
                backend.write_error = Some(io::ErrorKind::BrokenPipe);
            }
            let mut serial = Serial::new(Irq, backend);
            assert!(
                matches!(serial.write(0, b'x'), Err(vm_superio::serial::Error::IOError(e)) if e.kind() == io::ErrorKind::BrokenPipe)
            );
        }
    }

    #[test]
    fn non_send_trait_object_and_stop_during_recv() {
        let backend: Box<dyn SerialIo> = Box::new(Memory {
            stop_on_recv: true,
            ..Memory::default()
        });
        let mut serial = Serial::new(Irq, backend);
        assert!(poll_input(&mut serial).unwrap());
    }
}
