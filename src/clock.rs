use crate::core::ClockState;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

pub struct ClockSource {
    fd: libc::c_int,
}

impl ClockSource {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_CLOEXEC | libc::TFD_NONBLOCK)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let source = Self { fd };
        source.rearm()?;
        Ok(source)
    }

    pub fn sample(&self) -> io::Result<ClockState> {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
        if unsafe { libc::localtime_r(&now.tv_sec, &mut local) }.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(ClockState {
            hour: local.tm_hour as u8,
            minute: local.tm_min as u8,
            day: local.tm_mday as u8,
            month: (local.tm_mon + 1) as u8,
        })
    }

    pub fn on_readable(&self) -> io::Result<ClockState> {
        let mut expirations = 0_u64;
        let result = unsafe {
            libc::read(
                self.fd,
                (&mut expirations as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of::<u64>(),
            )
        };
        if result != std::mem::size_of::<u64>() as isize {
            return Err(io::Error::last_os_error());
        }
        let state = self.sample()?;
        self.rearm()?;
        Ok(state)
    }

    fn rearm(&self) -> io::Result<()> {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let deadline = next_minute_deadline(now.tv_sec);
        let timer = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: deadline,
                tv_nsec: 0,
            },
        };
        if unsafe {
            libc::timerfd_settime(
                self.fd,
                libc::TFD_TIMER_ABSTIME,
                &timer,
                std::ptr::null_mut(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl AsRawFd for ClockSource {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for ClockSource {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

pub fn next_minute_delay(seconds: i64) -> i64 {
    60 - seconds.rem_euclid(60)
}

pub fn next_minute_deadline(seconds: i64) -> i64 {
    seconds + next_minute_delay(seconds)
}

#[cfg(test)]
mod tests {
    use super::{next_minute_deadline, next_minute_delay};

    #[test]
    fn aligns_deadline_to_next_minute_without_drift() {
        assert_eq!(next_minute_delay(18 * 3600 + 42 * 60), 60);
        assert_eq!(next_minute_delay(18 * 3600 + 42 * 60 + 1), 59);
        assert_eq!(next_minute_delay(18 * 3600 + 42 * 60 + 37), 23);
        assert_eq!(next_minute_delay(18 * 3600 + 42 * 60 + 59), 1);
        assert_eq!(
            next_minute_deadline(18 * 3600 + 42 * 60 + 37),
            18 * 3600 + 43 * 60
        );
    }
}
