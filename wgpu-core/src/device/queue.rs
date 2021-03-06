/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[cfg(feature = "trace")]
use crate::device::trace::Action;
use crate::{
    command::{CommandAllocator, CommandBuffer},
    hub::{GfxBackend, Global, GlobalIdentityHandlerFactory, Token},
    id,
    resource::{BufferMapState, BufferUse},
};

use gfx_memory::{Block, Heaps, MemoryBlock};
use hal::{command::CommandBuffer as _, device::Device as _, queue::CommandQueue as _};
use smallvec::SmallVec;
use std::{iter, sync::atomic::Ordering};

#[derive(Debug, Default)]
pub(crate) struct PendingWrites<B: hal::Backend> {
    pub command_buffer: Option<B::CommandBuffer>,
    pub temp_buffers: Vec<(B::Buffer, MemoryBlock<B>)>,
}

impl<B: hal::Backend> PendingWrites<B> {
    pub fn new() -> Self {
        PendingWrites {
            command_buffer: None,
            temp_buffers: Vec::new(),
        }
    }

    pub fn dispose(
        self,
        device: &B::Device,
        com_allocator: &CommandAllocator<B>,
        mem_allocator: &mut Heaps<B>,
    ) {
        if let Some(raw) = self.command_buffer {
            com_allocator.discard_internal(raw);
        }
        for (buffer, memory) in self.temp_buffers {
            mem_allocator.free(device, memory);
            unsafe {
                device.destroy_buffer(buffer);
            }
        }
    }
}

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn queue_write_buffer<B: GfxBackend>(
        &self,
        queue_id: id::QueueId,
        data: &[u8],
        buffer_id: id::BufferId,
        buffer_offset: wgt::BufferAddress,
    ) {
        let hub = B::hub(self);
        let mut token = Token::root();
        let (mut device_guard, mut token) = hub.devices.write(&mut token);
        let device = &mut device_guard[queue_id];
        let (buffer_guard, _) = hub.buffers.read(&mut token);

        #[cfg(feature = "trace")]
        match device.trace {
            Some(ref trace) => {
                let mut trace = trace.lock();
                let data_path = trace.make_binary("bin", data);
                trace.add(Action::WriteBuffer {
                    id: buffer_id,
                    data: data_path,
                    range: buffer_offset..buffer_offset + data.len() as wgt::BufferAddress,
                    queued: true,
                });
            }
            None => {}
        }

        let mut trackers = device.trackers.lock();
        let (dst, transition) =
            trackers
                .buffers
                .use_replace(&*buffer_guard, buffer_id, (), BufferUse::COPY_DST);
        assert!(
            dst.usage.contains(wgt::BufferUsage::COPY_DST),
            "Write buffer usage {:?} must contain usage flag DST_SRC",
            dst.usage
        );

        let last_submit_index = device.life_guard.submission_index.load(Ordering::Relaxed);
        dst.life_guard.use_at(last_submit_index + 1);

        let mut src_raw = unsafe {
            device
                .raw
                .create_buffer(
                    data.len() as wgt::BufferAddress,
                    hal::buffer::Usage::TRANSFER_SRC,
                )
                .unwrap()
        };
        //TODO: do we need to transition into HOST_WRITE access first?
        let requirements = unsafe { device.raw.get_buffer_requirements(&src_raw) };

        let mut memory = device
            .mem_allocator
            .lock()
            .allocate(
                &device.raw,
                &requirements,
                gfx_memory::MemoryUsage::Staging { read_back: false },
                gfx_memory::Kind::Linear,
            )
            .unwrap();
        unsafe {
            device
                .raw
                .set_buffer_name(&mut src_raw, "<write_buffer_temp>");
            device
                .raw
                .bind_buffer_memory(memory.memory(), memory.segment().offset, &mut src_raw)
                .unwrap();
        }

        let mut mapped = memory.map(&device.raw, hal::memory::Segment::ALL).unwrap();
        unsafe { mapped.write(&device.raw, hal::memory::Segment::ALL) }
            .unwrap()
            .slice[..data.len()]
            .copy_from_slice(data);

        let mut comb = match device.pending_writes.command_buffer.take() {
            Some(comb) => comb,
            None => {
                let mut comb = device.com_allocator.allocate_internal();
                unsafe {
                    comb.begin_primary(hal::command::CommandBufferFlags::ONE_TIME_SUBMIT);
                }
                comb
            }
        };
        let region = hal::command::BufferCopy {
            src: 0,
            dst: buffer_offset,
            size: data.len() as _,
        };
        unsafe {
            comb.pipeline_barrier(
                super::all_buffer_stages()..hal::pso::PipelineStage::TRANSFER,
                hal::memory::Dependencies::empty(),
                iter::once(hal::memory::Barrier::Buffer {
                    states: hal::buffer::Access::HOST_WRITE..hal::buffer::Access::TRANSFER_READ,
                    target: &src_raw,
                    range: hal::buffer::SubRange::WHOLE,
                    families: None,
                })
                .chain(transition.map(|pending| pending.into_hal(dst))),
            );
            comb.copy_buffer(&src_raw, &dst.raw, iter::once(region));
        }
        device.pending_writes.temp_buffers.push((src_raw, memory));
        device.pending_writes.command_buffer = Some(comb);
    }

    pub fn queue_submit<B: GfxBackend>(
        &self,
        queue_id: id::QueueId,
        command_buffer_ids: &[id::CommandBufferId],
    ) {
        let hub = B::hub(self);

        let callbacks = {
            let mut token = Token::root();
            let (mut device_guard, mut token) = hub.devices.write(&mut token);
            let device = &mut device_guard[queue_id];
            let pending_write_command_buffer =
                device
                    .pending_writes
                    .command_buffer
                    .take()
                    .map(|mut comb_raw| unsafe {
                        comb_raw.finish();
                        comb_raw
                    });
            device.temp_suspected.clear();

            let submit_index = 1 + device
                .life_guard
                .submission_index
                .fetch_add(1, Ordering::Relaxed);

            let fence = {
                let mut signal_swapchain_semaphores = SmallVec::<[_; 1]>::new();
                let (mut swap_chain_guard, mut token) = hub.swap_chains.write(&mut token);
                let (mut command_buffer_guard, mut token) = hub.command_buffers.write(&mut token);

                {
                    let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
                    let (compute_pipe_guard, mut token) = hub.compute_pipelines.read(&mut token);
                    let (render_pipe_guard, mut token) = hub.render_pipelines.read(&mut token);
                    let (mut buffer_guard, mut token) = hub.buffers.write(&mut token);
                    let (texture_guard, mut token) = hub.textures.read(&mut token);
                    let (texture_view_guard, mut token) = hub.texture_views.read(&mut token);
                    let (sampler_guard, _) = hub.samplers.read(&mut token);

                    //Note: locking the trackers has to be done after the storages
                    let mut trackers = device.trackers.lock();

                    //TODO: if multiple command buffers are submitted, we can re-use the last
                    // native command buffer of the previous chain instead of always creating
                    // a temporary one, since the chains are not finished.

                    // finish all the command buffers first
                    for &cmb_id in command_buffer_ids {
                        let comb = &mut command_buffer_guard[cmb_id];
                        #[cfg(feature = "trace")]
                        match device.trace {
                            Some(ref trace) => trace
                                .lock()
                                .add(Action::Submit(submit_index, comb.commands.take().unwrap())),
                            None => (),
                        };

                        if let Some((sc_id, fbo)) = comb.used_swap_chain.take() {
                            let sc = &mut swap_chain_guard[sc_id.value];
                            assert!(sc.acquired_view_id.is_some(),
                                "SwapChainOutput for {:?} was dropped before the respective command buffer {:?} got submitted!",
                                sc_id.value, cmb_id);
                            if sc.acquired_framebuffers.is_empty() {
                                signal_swapchain_semaphores.push(sc_id.value);
                            }
                            sc.acquired_framebuffers.push(fbo);
                        }

                        // optimize the tracked states
                        comb.trackers.optimize();

                        // update submission IDs
                        for id in comb.trackers.buffers.used() {
                            if let BufferMapState::Waiting(_) = buffer_guard[id].map_state {
                                panic!("Buffer has a pending mapping.");
                            }
                            if !buffer_guard[id].life_guard.use_at(submit_index) {
                                if let BufferMapState::Active { .. } = buffer_guard[id].map_state {
                                    log::warn!("Dropped buffer has a pending mapping.");
                                    super::unmap_buffer(&device.raw, &mut buffer_guard[id]);
                                }
                                device.temp_suspected.buffers.push(id);
                            }
                        }
                        for id in comb.trackers.textures.used() {
                            if !texture_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.textures.push(id);
                            }
                        }
                        for id in comb.trackers.views.used() {
                            if !texture_view_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.texture_views.push(id);
                            }
                        }
                        for id in comb.trackers.bind_groups.used() {
                            if !bind_group_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.bind_groups.push(id);
                            }
                        }
                        for id in comb.trackers.samplers.used() {
                            if !sampler_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.samplers.push(id);
                            }
                        }
                        for id in comb.trackers.compute_pipes.used() {
                            if !compute_pipe_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.compute_pipelines.push(id);
                            }
                        }
                        for id in comb.trackers.render_pipes.used() {
                            if !render_pipe_guard[id].life_guard.use_at(submit_index) {
                                device.temp_suspected.render_pipelines.push(id);
                            }
                        }

                        // execute resource transitions
                        let mut transit = device.com_allocator.extend(comb);
                        unsafe {
                            // the last buffer was open, closing now
                            comb.raw.last_mut().unwrap().finish();
                            transit
                                .begin_primary(hal::command::CommandBufferFlags::ONE_TIME_SUBMIT);
                        }
                        log::trace!("Stitching command buffer {:?} before submission", cmb_id);
                        CommandBuffer::insert_barriers(
                            &mut transit,
                            &mut *trackers,
                            &comb.trackers,
                            &*buffer_guard,
                            &*texture_guard,
                        );
                        unsafe {
                            transit.finish();
                        }
                        comb.raw.insert(0, transit);
                    }

                    log::debug!("Device after submission {}: {:#?}", submit_index, trackers);
                }

                // now prepare the GPU submission
                let fence = device.raw.create_fence(false).unwrap();
                let submission = hal::queue::Submission {
                    command_buffers: pending_write_command_buffer.as_ref().into_iter().chain(
                        command_buffer_ids
                            .iter()
                            .flat_map(|&cmb_id| &command_buffer_guard[cmb_id].raw),
                    ),
                    wait_semaphores: Vec::new(),
                    signal_semaphores: signal_swapchain_semaphores
                        .into_iter()
                        .map(|sc_id| &swap_chain_guard[sc_id].semaphore),
                };

                unsafe {
                    device.queue_group.queues[0].submit(submission, Some(&fence));
                }
                fence
            };

            if let Some(comb_raw) = pending_write_command_buffer {
                device
                    .com_allocator
                    .after_submit_internal(comb_raw, submit_index);
            }

            let callbacks = device.maintain(self, false, &mut token);
            super::Device::lock_life_internal(&device.life_tracker, &mut token).track_submission(
                submit_index,
                fence,
                &device.temp_suspected,
                device.pending_writes.temp_buffers.drain(..),
            );

            // finally, return the command buffers to the allocator
            for &cmb_id in command_buffer_ids {
                let (cmd_buf, _) = hub.command_buffers.unregister(cmb_id, &mut token);
                device.com_allocator.after_submit(cmd_buf, submit_index);
            }

            callbacks
        };

        super::fire_map_callbacks(callbacks);
    }
}
