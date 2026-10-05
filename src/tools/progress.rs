use anyhow::{Result, bail};
use std::{
    io::{Read, Write},
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub enum Progress {
    Stage(String),
    Download {
        bytes: u64,
        total: Option<u64>,
        bytes_per_second: f64,
    },
    Output(String),
}

impl Progress {
    pub fn fraction(&self) -> Option<f32> {
        match self {
            Self::Download {
                bytes,
                total: Some(total),
                ..
            } if *total > 0 => Some((*bytes as f64 / *total as f64).min(1.0) as f32),
            _ => None,
        }
    }
}
impl std::fmt::Display for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stage(s) | Self::Output(s) => f.write_str(s),
            Self::Download {
                bytes,
                total,
                bytes_per_second,
            } => {
                write!(f, "已下载 {:.1} MiB", *bytes as f64 / 1048576.0)?;
                if let Some(total) = total {
                    write!(
                        f,
                        " / {:.1} MiB ({:.0}%)",
                        *total as f64 / 1048576.0,
                        self.fraction().unwrap_or(0.0) * 100.0
                    )?;
                }
                write!(f, " · {:.1} MiB/s", bytes_per_second / 1048576.0)
            }
        }
    }
}

pub fn transfer(
    mut input: impl Read,
    mut output: impl Write,
    total: Option<u64>,
    progress: &mut impl FnMut(Progress),
) -> Result<()> {
    const LIMIT: u64 = 256 * 1024 * 1024;
    if total.is_some_and(|n| n > LIMIT) {
        bail!("tool download is too large");
    }
    let start = Instant::now();
    let mut last = start;
    let mut bytes = 0;
    let mut buffer = [0; 64 * 1024];
    progress(Progress::Download {
        bytes,
        total,
        bytes_per_second: 0.0,
    });
    loop {
        let count = match input.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > LIMIT {
            bail!("tool download is too large");
        }
        output.write_all(&buffer[..count])?;
        if last.elapsed() >= Duration::from_millis(100) {
            progress(Progress::Download {
                bytes,
                total,
                bytes_per_second: bytes as f64 / start.elapsed().as_secs_f64().max(0.001),
            });
            last = Instant::now();
        }
    }
    if total.is_some_and(|n| n != bytes) {
        bail!("tool download was truncated");
    }
    output.flush()?;
    progress(Progress::Download {
        bytes,
        total,
        bytes_per_second: bytes as f64 / start.elapsed().as_secs_f64().max(0.001),
    });
    Ok(())
}

pub struct ProgressReader<R, F: FnMut(Progress)> {
    inner: R,
    callback: F,
    total: Option<u64>,
    bytes: u64,
    start: Instant,
    last: Instant,
}
impl<R, F: FnMut(Progress)> ProgressReader<R, F> {
    pub fn new(inner: R, total: Option<u64>, callback: F) -> Self {
        let now = Instant::now();
        let mut reader = Self {
            inner,
            callback,
            total,
            bytes: 0,
            start: now,
            last: now,
        };
        reader.emit();
        reader
    }
    fn emit(&mut self) {
        (self.callback)(Progress::Download {
            bytes: self.bytes,
            total: self.total,
            bytes_per_second: self.bytes as f64 / self.start.elapsed().as_secs_f64().max(0.001),
        });
        self.last = Instant::now();
    }
}
impl<R: Read, F: FnMut(Progress)> Read for ProgressReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.bytes += count as u64;
        if count == 0 || self.last.elapsed() >= Duration::from_millis(100) {
            self.emit();
        }
        Ok(count)
    }
}
impl<R, F: FnMut(Progress)> Drop for ProgressReader<R, F> {
    fn drop(&mut self) {
        self.emit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recoverable_read_interruption_does_not_abort_download() {
        struct Once(bool);
        impl Read for Once {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    return Err(std::io::ErrorKind::Interrupted.into());
                }
                self.0 = false;
                if b.is_empty() {
                    return Ok(0);
                }
                b[0] = 42;
                Ok(1)
            }
        }
        let mut output = Vec::new();
        transfer(Once(false).take(1), &mut output, Some(1), &mut |_| {}).unwrap();
        assert_eq!(output, [42]);
        let mut events = Vec::new();
        let mut reader = ProgressReader::new(&b"abc"[..], Some(3), |p| events.push(p));
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        drop(reader);
        assert!(matches!(
            events.last(),
            Some(Progress::Download { bytes: 3, .. })
        ));
        for total in [None, Some(3)] {
            let mut events = Vec::new();
            let mut reader = ProgressReader::new(&b"abc"[..], total, |p| events.push(p));
            reader.read_exact(&mut [0; 1]).unwrap();
            drop(reader);
            assert!(
                matches!(events.last(), Some(Progress::Download { bytes: 1, total: t, .. }) if *t == total)
            );
        }
    }
    #[test]
    fn transfer_reports_real_bytes_without_inventing_unknown_totals() {
        for total in [None, Some(70000)] {
            let mut events = Vec::new();
            let mut output = Vec::new();
            transfer(
                std::io::Cursor::new(vec![42; 70000]),
                &mut output,
                total,
                &mut |p| events.push(p),
            )
            .unwrap();
            assert_eq!(output.len(), 70000);
            assert!(
                matches!(events.last(), Some(Progress::Download { bytes: 70000, total: t, .. }) if *t == total)
            );
            assert_eq!(events.last().unwrap().fraction(), total.map(|_| 1.0));
        }
        assert!(transfer(&b"short"[..], Vec::new(), Some(6), &mut |_| {}).is_err());
    }
}
