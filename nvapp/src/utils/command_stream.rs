use nvgpu::*;
use super::GpuAllocated;

use std::fmt::Debug;

use std::mem::ManuallyDrop;

#[derive(Debug, PartialEq)]
pub enum CommandSubmissionMode {
    /// ?
    IncreasingOld,

    /// Tells PFIFO to read as much arguments as specified by argument count, while automatically incrementing the method value.
    /// This means that each argument will be written to a different method location.
    Increasing,

    /// ?
    NonIncreasingOld,

    /// Tells PFIFO to read as much arguments as specified by argument count.
    /// However, all arguments will be written to the same method location.
    NonIncreasing,

    /// Tells PFIFO to read inline data from bits 28-16 of the command word, thus eliminating the need to pass additional words for the arguments.
    Inline,

    /// Tells PFIFO to read as much arguments as specified by argument count and automatically increments the method value once only.
    IncreasingOnce,
}

enum_with_val! {
    #[derive(PartialEq, Eq, Clone, Copy)]
    pub struct SubChannelId(pub u32) {
        THREE_D = 0,
        COMPUTE = 1,
        INLINE_TO_MEMORY = 2,
        TWO_D = 3,
        DMA = 4,
    }
}

pub struct Command {
    entry: GpFifoEntry,
    submission_mode: CommandSubmissionMode,
    arguments: Vec<u32>,
}

impl Command {
    pub fn new(method: u32, sub_channel: SubChannelId, submission_mode: CommandSubmissionMode) -> Self {
        Self::new_raw(method, sub_channel.0, submission_mode)
    }

    pub fn new_raw(method: u32, sub_channel: u32, submission_mode: CommandSubmissionMode) -> Self {
        let mut res = Command {
            entry: GpFifoEntry(0),
            submission_mode,
            arguments: Vec::new(),
        };

        res.entry.set_method(method);
        res.entry.set_sub_channel(sub_channel);

        let submission_mode_id = match res.submission_mode {
            CommandSubmissionMode::IncreasingOld => 0,
            CommandSubmissionMode::Increasing => 1,
            CommandSubmissionMode::NonIncreasingOld => 2,
            CommandSubmissionMode::NonIncreasing => 3,
            CommandSubmissionMode::Inline => 4,
            CommandSubmissionMode::IncreasingOnce => 5,
        };

        res.entry.set_submission_mode(submission_mode_id);

        res
    }

    pub fn new_inline(method: u32, sub_channel: u32, arguments: u32) -> Self {
        let mut res = Self::new_raw(method, sub_channel, CommandSubmissionMode::Inline);
        res.entry.set_inline_arguments(arguments);

        res
    }

    pub fn push_argument(&mut self, argument: u32) {
        assert!(self.submission_mode != CommandSubmissionMode::Inline);
        self.arguments.push(argument);
    }

    pub fn push_address(&mut self, address: GpuVirtualAddress) {
        self.push_argument((address >> 32) as u32);
        self.push_argument(address as u32);
    }

    pub fn into_vec(mut self) -> Vec<u32> {
        let mut res = Vec::new();

        self.entry.set_argument_count(self.arguments.len() as u32);

        res.push(self.entry.0);
        res.append(&mut self.arguments);

        res
    }

    pub fn into_gpu_allocated(self) -> NvGpuResult<GpuAllocated> {
        let vec = self.into_vec();

        let res = GpuAllocated::new(vec.len() * std::mem::size_of::<u32>(), 0x20000)?;

        let arguments: &mut [u32] = res.map_array_mut()?;
        arguments.copy_from_slice(&vec[..]);

        res.flush()?;
        res.unmap()?;

        Ok(res)
    }
}

pub struct CommandStream<'a> {
    /// the inner implementation.
    fifo: ManuallyDrop<GpFifoQueue<'a>>,

    /// A Vec containing allocation to use in fifo.
    command_list: Vec<Command>,

    /// The previous command buffers kept alive to avoid being unmap by Drop during processing of the GPFIFO.
    in_process: ManuallyDrop<Vec<GpuAllocated>>,
}

impl<'a> Drop for CommandStream<'a> {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.fifo);
            ManuallyDrop::drop(&mut self.in_process);
        }
    }
}

impl<'a> CommandStream<'a> {
    pub fn new(channel: &'a Channel) -> Self {
        CommandStream {
            fifo: ManuallyDrop::new(GpFifoQueue::new(channel)),
            command_list: Vec::new(),
            in_process: ManuallyDrop::new(Vec::new()),
        }
    }

    pub fn push(&mut self, command: Command) -> NvGpuResult<()> {
        self.command_list.push(command);

        Ok(())
    }

    pub fn flush(&mut self) -> NvGpuResult<()> {
        let mut commands = Vec::new();

        for command in self.command_list.drain(..) {
            commands.append(&mut command.into_vec());
        }

        let commands_gpu = GpuAllocated::new(commands.len() * std::mem::size_of::<u32>(), 0x20000)?;

        let fifo_array: &mut [u32] = commands_gpu.map_array_mut()?;
        fifo_array.copy_from_slice(&commands[..]);

        commands_gpu.flush()?;
        commands_gpu.unmap()?;
        self.fifo.append(
            commands_gpu.gpu_address(),
            (commands_gpu.user_size() as u64) / 4,
            0,
        );

        self.in_process.push(commands_gpu);
        self.fifo.submit()?;

        Ok(())
    }

    pub fn wait_idle(&mut self) {
        self.fifo.wait_idle().unwrap();
    }
}

pub fn setup_channel(stream : &mut CommandStream) -> NvGpuResult<()> {
    // Bind subchannel 0, 3D
    let mut bind_channel_command = Command::new(0, SubChannelId::THREE_D, CommandSubmissionMode::Increasing);
    bind_channel_command.push_argument(ClassId::MAXWELL_B_3D.0);
    stream.push(bind_channel_command)?;

    // Bind subchannel 1, Compute
    let mut bind_channel_command = Command::new(0, SubChannelId::COMPUTE, CommandSubmissionMode::Increasing);
    bind_channel_command.push_argument(ClassId::MAXWELL_B_COMPUTE.0);
    stream.push(bind_channel_command)?;

    // Bind subchannel 2, Inline To Memory
    let mut bind_channel_command = Command::new(0, SubChannelId::INLINE_TO_MEMORY, CommandSubmissionMode::Increasing);
    bind_channel_command.push_argument(ClassId::INLINE_TO_MEMORY.0);
    stream.push(bind_channel_command)?;

    // Bind subchannel 3, 2D
    let mut bind_channel_command = Command::new(0, SubChannelId::TWO_D, CommandSubmissionMode::Increasing);
    bind_channel_command.push_argument(ClassId::MAXWELL_A_2D.0);
    stream.push(bind_channel_command)?;

    // Bind subchannel 4, DMA
    let mut bind_channel_command = Command::new(0, SubChannelId::DMA, CommandSubmissionMode::Increasing);
    bind_channel_command.push_argument(ClassId::MAXWELL_B_DMA.0);
    stream.push(bind_channel_command)?;

    stream.wait_idle();

    Ok(())
}