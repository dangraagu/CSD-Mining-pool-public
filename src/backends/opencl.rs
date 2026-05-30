//! OpenCL mining backend — 2-queue pipelined.
//!
//! Two command queues alternate launches. While queue A's batch is
//! running on the GPU, the host reads back queue B's previous result
//! (and decides whether to stop or queue the next launch on B). This
//! keeps the device almost continuously busy.
//!
//! Geometry: each launch dispatches `blocks * threads_per_block * 1`
//! work-items; the kernel itself sweeps `nonces_per_thread` nonces per
//! work-item. So total nonces per launch =
//!   blocks * threads_per_block * nonces_per_thread

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Result};
use opencl3::command_queue::{CommandQueue, CL_QUEUE_PROFILING_ENABLE};
use opencl3::context::Context;
use opencl3::device::{get_all_devices, Device, CL_DEVICE_TYPE_GPU};
use opencl3::kernel::{ExecuteKernel, Kernel};
use opencl3::memory::{Buffer, CL_MEM_READ_ONLY, CL_MEM_WRITE_ONLY};
use opencl3::program::Program;
use opencl3::types::{cl_uchar, cl_uint, CL_BLOCKING};

use crate::backend::{MiningBackend, MiningResult};
use crate::sha256d_cpu::midstate_of_first_chunk_fast as midstate_of_first_chunk;

const KERNEL_SRC: &str = include_str!("../kernels/sha256d.cl");
const KERNEL_NAME: &str = "mine_sha256d";

pub struct OpenclBackend {
    context: Context,
    program: Program,
    queue_a: CommandQueue,
    queue_b: CommandQueue,
    pub blocks: u32,
    pub threads_per_block: u32,
    pub nonces_per_thread: u32,
}

impl OpenclBackend {
    pub fn new(
        device_index: usize,
        blocks: u32,
        threads_per_block: u32,
        nonces_per_thread: u32,
    ) -> Result<Self> {
        let devices = get_all_devices(CL_DEVICE_TYPE_GPU)
            .map_err(|e| anyhow!("opencl: get_all_devices failed: {:?}", e))?;
        if devices.is_empty() {
            bail!("opencl: no GPU devices found");
        }
        if device_index >= devices.len() {
            bail!(
                "opencl: no GPU at --device {} (found {} OpenCL GPU(s): valid indices 0..={})",
                device_index,
                devices.len(),
                devices.len() - 1
            );
        }
        let device = Device::new(devices[device_index]);
        let name = device.name().unwrap_or_default();
        tracing::info!("opencl device: {}", name);

        let context = Context::from_device(&device)
            .map_err(|e| anyhow!("opencl: create context failed: {:?}", e))?;

        // NOTE: `create_default` is deprecated in opencl3 0.9.5 in favor of
        // `create_default_with_properties` (CL_VERSION_2_0). Migration would
        // change the queue-size semantics and risk perturbing a verified hot
        // path; keep the legacy API and silence the warning until we have a
        // tested migration path.
        #[allow(deprecated)]
        let queue_a = CommandQueue::create_default(&context, CL_QUEUE_PROFILING_ENABLE)
            .map_err(|e| anyhow!("opencl: create queue_a failed: {:?}", e))?;
        #[allow(deprecated)]
        let queue_b = CommandQueue::create_default(&context, CL_QUEUE_PROFILING_ENABLE)
            .map_err(|e| anyhow!("opencl: create queue_b failed: {:?}", e))?;

        let program = Program::create_and_build_from_source(&context, KERNEL_SRC, "")
            .map_err(|e| anyhow!("opencl: build kernel failed: {}", e))?;

        Ok(Self {
            context,
            program,
            queue_a,
            queue_b,
            blocks: blocks.max(1),
            threads_per_block: threads_per_block.max(1),
            nonces_per_thread: nonces_per_thread.max(1),
        })
    }

    fn nonces_per_launch(&self) -> u64 {
        self.blocks as u64 * self.threads_per_block as u64 * self.nonces_per_thread as u64
    }
}

/// Per-queue resources. One kernel + the four input/output buffers.
struct PipeRes {
    kernel: Kernel,
    mid_buf: Buffer<cl_uint>,
    tail_buf: Buffer<cl_uchar>,
    target_buf: Buffer<cl_uchar>,
    found_nonce: Buffer<cl_uint>,
    found_flag: Buffer<cl_uint>,
    found_hash: Buffer<cl_uint>,
    /// Nonce offset for the launch currently in-flight on this queue
    /// (only meaningful while `in_flight` is true).
    pending_start: u32,
    in_flight: bool,
}

impl PipeRes {
    fn new(ctx: &Context, prog: &Program) -> Result<Self> {
        let kernel = Kernel::create(prog, KERNEL_NAME)
            .map_err(|e| anyhow!("opencl: create kernel failed: {:?}", e))?;
        let (mid_buf, tail_buf, target_buf, found_nonce, found_flag, found_hash) = unsafe {
            (
                Buffer::<cl_uint>::create(ctx, CL_MEM_READ_ONLY, 8, ptr::null_mut())
                    .map_err(|e| anyhow!("opencl: mid_buf: {:?}", e))?,
                Buffer::<cl_uchar>::create(ctx, CL_MEM_READ_ONLY, 16, ptr::null_mut())
                    .map_err(|e| anyhow!("opencl: tail_buf: {:?}", e))?,
                Buffer::<cl_uchar>::create(ctx, CL_MEM_READ_ONLY, 32, ptr::null_mut())
                    .map_err(|e| anyhow!("opencl: target_buf: {:?}", e))?,
                Buffer::<cl_uint>::create(ctx, CL_MEM_WRITE_ONLY, 1, ptr::null_mut())
                    .map_err(|e| anyhow!("opencl: found_nonce: {:?}", e))?,
                Buffer::<cl_uint>::create(ctx, CL_MEM_WRITE_ONLY, 1, ptr::null_mut())
                    .map_err(|e| anyhow!("opencl: found_flag: {:?}", e))?,
                Buffer::<cl_uint>::create(ctx, CL_MEM_WRITE_ONLY, 8, ptr::null_mut())
                    .map_err(|e| anyhow!("opencl: found_hash: {:?}", e))?,
            )
        };
        Ok(Self {
            kernel,
            mid_buf,
            tail_buf,
            target_buf,
            found_nonce,
            found_flag,
            found_hash,
            pending_start: 0,
            in_flight: false,
        })
    }
}

impl MiningBackend for OpenclBackend {
    fn name(&self) -> &'static str {
        "opencl"
    }

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &AtomicBool,
    ) -> Option<MiningResult> {
        if nonce_end <= nonce_start {
            return None;
        }

        let midstate = midstate_of_first_chunk(&header_84);
        let midstate_words: [cl_uint; 8] = [
            midstate[0], midstate[1], midstate[2], midstate[3],
            midstate[4], midstate[5], midstate[6], midstate[7],
        ];
        let mut tail_16 = [0u8; 16];
        tail_16.copy_from_slice(&header_84[64..80]);

        let mut a = match PipeRes::new(&self.context, &self.program) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("opencl: pipe A setup failed: {}", e);
                return None;
            }
        };
        let mut b = match PipeRes::new(&self.context, &self.program) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("opencl: pipe B setup failed: {}", e);
                return None;
            }
        };

        // Upload constants once per pipe.
        for (queue, pipe) in [(&self.queue_a, &mut a), (&self.queue_b, &mut b)] {
            unsafe {
                queue
                    .enqueue_write_buffer(&mut pipe.mid_buf, CL_BLOCKING, 0, &midstate_words, &[])
                    .map_err(|e| anyhow!("write midstate: {:?}", e))
                    .ok()?;
                queue
                    .enqueue_write_buffer(&mut pipe.tail_buf, CL_BLOCKING, 0, &tail_16, &[])
                    .map_err(|e| anyhow!("write tail: {:?}", e))
                    .ok()?;
                queue
                    .enqueue_write_buffer(&mut pipe.target_buf, CL_BLOCKING, 0, &target, &[])
                    .map_err(|e| anyhow!("write target: {:?}", e))
                    .ok()?;
            }
        }

        let nonces_per_launch = self.nonces_per_launch();
        let local_size = self.threads_per_block as usize;
        let global = (self.blocks as usize) * local_size;
        let mut next_start: u64 = nonce_start as u64;
        let mut current_pipe = 0u8;

        loop {
            if stop.load(Ordering::Relaxed) {
                return None;
            }

            // 1) Drain the current pipe if in flight (no borrow of "other"
            //    needed yet — keeps the borrow checker happy).
            let drain_result = {
                let (pipe, queue) = pick_pipe(&mut a, &mut b, current_pipe, &self.queue_a, &self.queue_b);
                if pipe.in_flight {
                    let res = drain_pipe(queue, pipe);
                    pipe.in_flight = false;
                    res
                } else {
                    None
                }
            };
            if let Some(res) = drain_result {
                // Drain the other pipe too so its in-flight work doesn't
                // race with the next job.
                let (other, oqueue) = pick_pipe(&mut a, &mut b, current_pipe ^ 1, &self.queue_a, &self.queue_b);
                if other.in_flight {
                    let _ = drain_pipe(oqueue, other);
                    other.in_flight = false;
                }
                return Some(res);
            }

            // 2) Launch the next batch on this pipe (if there's nonce space left).
            let launched = {
                let (pipe, queue) = pick_pipe(&mut a, &mut b, current_pipe, &self.queue_a, &self.queue_b);
                if next_start < nonce_end as u64 {
                    let launch_size = nonces_per_launch.min((nonce_end as u64) - next_start);
                    if launch_size > 0 {
                        let start_u32 = next_start as u32;
                        let zero = [0u32];
                        unsafe {
                            if queue
                                .enqueue_write_buffer(
                                    &mut pipe.found_flag,
                                    CL_BLOCKING,
                                    0,
                                    &zero,
                                    &[],
                                )
                                .is_err()
                            {
                                return None;
                            }
                        }
                        let end_u32 = nonce_end; // hard cap respected by kernel
                        unsafe {
                            if ExecuteKernel::new(&pipe.kernel)
                                .set_arg(&pipe.mid_buf)
                                .set_arg(&pipe.tail_buf)
                                .set_arg(&pipe.target_buf)
                                .set_arg(&start_u32)
                                .set_arg(&end_u32)
                                .set_arg(&self.nonces_per_thread)
                                .set_arg(&pipe.found_nonce)
                                .set_arg(&pipe.found_flag)
                                .set_arg(&pipe.found_hash)
                                .set_global_work_size(global)
                                .set_local_work_size(local_size)
                                .enqueue_nd_range(queue)
                                .is_err()
                            {
                                return None;
                            }
                        }
                        pipe.pending_start = start_u32;
                        pipe.in_flight = true;
                        next_start = next_start.saturating_add(launch_size);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            // 3) If we couldn't launch and neither pipe is busy, the whole
            //    nonce range is exhausted.
            if !launched && !a.in_flight && !b.in_flight {
                return None;
            }

            current_pipe ^= 1;
        }
    }
}

/// Pick the &mut PipeRes for the requested side, plus its CommandQueue.
fn pick_pipe<'a>(
    a: &'a mut PipeRes,
    b: &'a mut PipeRes,
    which: u8,
    qa: &'a CommandQueue,
    qb: &'a CommandQueue,
) -> (&'a mut PipeRes, &'a CommandQueue) {
    if which == 0 {
        (a, qa)
    } else {
        (b, qb)
    }
}

/// Wait for `pipe`'s pending launch and read out the flag/nonce/hash.
fn drain_pipe(queue: &CommandQueue, pipe: &mut PipeRes) -> Option<MiningResult> {
    let _ = queue.finish();
    let mut flag = [0u32; 1];
    unsafe {
        queue
            .enqueue_read_buffer(&pipe.found_flag, CL_BLOCKING, 0, &mut flag, &[])
            .ok()?;
    }
    if flag[0] == 0 {
        return None;
    }
    let mut nonce_out = [0u32; 1];
    let mut hash_words = [0u32; 8];
    unsafe {
        queue
            .enqueue_read_buffer(&pipe.found_nonce, CL_BLOCKING, 0, &mut nonce_out, &[])
            .ok()?;
        queue
            .enqueue_read_buffer(&pipe.found_hash, CL_BLOCKING, 0, &mut hash_words, &[])
            .ok()?;
    }
    let mut hash = [0u8; 32];
    for i in 0..8 {
        let be = hash_words[i].to_be_bytes();
        hash[4 * i..4 * i + 4].copy_from_slice(&be);
    }
    Some(MiningResult {
        nonce: nonce_out[0],
        hash,
    })
}
