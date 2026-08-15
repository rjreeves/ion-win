//! Private RAII wrapper for Windows Job Objects.
//!
//! Job Objects are an optional execution backend facility. Closing this
//! wrapper preserves assigned processes by default; process-tree termination
//! is always an explicit policy action, which keeps `disown` safe.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::io::AsRawHandle;
use std::process::Child;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosePolicy {
    PreserveProcesses,
    TerminateProcesses,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JobAccounting {
    pub total_processes: u32,
    pub active_processes: u32,
    pub terminated_processes: u32,
}

pub struct JobObject {
    handle: HANDLE,
    close_policy: ClosePolicy,
}

impl JobObject {
    pub fn new(close_policy: ClosePolicy) -> io::Result<Self> {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle == 0 {
            return Err(io::Error::last_os_error());
        }
        let job = Self {
            handle,
            close_policy,
        };
        if close_policy == ClosePolicy::TerminateProcesses {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(job)
    }

    pub fn assign(&self, child: &Child) -> io::Result<()> {
        let process = child.as_raw_handle() as HANDLE;
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        if assigned == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn accounting(&self) -> io::Result<JobAccounting> {
        let mut data: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicAccountingInformation,
                &mut data as *mut _ as *mut _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(JobAccounting {
            total_processes: data.TotalProcesses,
            active_processes: data.ActiveProcesses,
            terminated_processes: data.TotalTerminatedProcesses,
        })
    }

    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        let terminated = unsafe { TerminateJobObject(self.handle, exit_code) };
        if terminated == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn close_policy(&self) -> ClosePolicy {
        self.close_policy
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sleeping_child() -> Child {
        std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "ping 127.0.0.1 -n 30 > nul"])
            .spawn()
            .unwrap()
    }

    #[test]
    fn new_job_starts_with_zero_process_accounting() {
        let job = JobObject::new(ClosePolicy::PreserveProcesses).unwrap();
        assert_eq!(job.close_policy(), ClosePolicy::PreserveProcesses);
        assert_eq!(job.accounting().unwrap(), JobAccounting::default());
    }

    #[test]
    fn preserve_policy_does_not_terminate_process_on_close() {
        let mut child = sleeping_child();
        let job = JobObject::new(ClosePolicy::PreserveProcesses).unwrap();
        job.assign(&child).unwrap();
        assert!(job.accounting().unwrap().active_processes >= 1);
        drop(job);
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn explicit_termination_stops_assigned_processes() {
        let mut child = sleeping_child();
        let job = JobObject::new(ClosePolicy::PreserveProcesses).unwrap();
        job.assign(&child).unwrap();
        job.terminate(1).unwrap();
        child.wait().unwrap();
    }
}
