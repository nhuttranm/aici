use std::ffi::CString;
use std::io;

use anyhow::Result;
use log::info;

pub struct Semaphore {
    sem: *mut libc::sem_t,
}

impl Semaphore {
    fn last_error<T>() -> Result<T> {
        Err(io::Error::last_os_error().into())
    }

    pub fn new(name: &str, initial_value: u32) -> Result<Self> {
        info!("sem_open: {}", name);
        let c_name = CString::new(name).unwrap();
        let sem = unsafe { libc::sem_open(c_name.as_ptr(), libc::O_CREAT, 0o666, initial_value) };

        if sem.is_null() {
            return Self::last_error();
        }

        Ok(Self { sem })
    }

    pub fn wait(&self) -> Result<()> {
        let ret = unsafe { libc::sem_wait(self.sem) };
        if ret < 0 {
            return Self::last_error();
        }
        Ok(())
    }

    pub fn post(&self) -> Result<()> {
        let ret = unsafe { libc::sem_post(self.sem) };
        if ret < 0 {
            return Self::last_error();
        }
        Ok(())
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        unsafe {
            libc::sem_close(self.sem);
        }
    }
}