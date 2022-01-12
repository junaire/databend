use std::collections::VecDeque;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use futures::{FutureExt, pin_mut};
use futures::future::BoxFuture;
use futures::task::{ArcWake, WakerRef};

use common_exception::ErrorCode;
use common_exception::Result;

use crate::pipelines::new::executor::executor_graph::RunningGraph;
use crate::pipelines::new::executor::executor_tasks::{ExecutingAsyncTask, ExecutorTasksQueue};
use crate::pipelines::new::processors::processor::ProcessorPtr;

pub enum ExecutorTask {
    None,
    Sync(ProcessorPtr),
    Async(ProcessorPtr),
    AsyncSchedule(ExecutingAsyncTask),
}

pub struct ExecutorWorkerContext {
    worker_num: usize,
    task: ExecutorTask,
}

impl ExecutorWorkerContext {
    pub fn create(worker_num: usize) -> Self {
        ExecutorWorkerContext {
            worker_num,
            task: ExecutorTask::None,
        }
    }

    pub fn has_task(&self) -> bool {
        !matches!(&self.task, ExecutorTask::None)
    }

    pub fn get_worker_num(&self) -> usize {
        self.worker_num
    }

    pub fn set_task(&mut self, task: ExecutorTask) {
        self.task = task
    }

    pub unsafe fn execute_task(&mut self, queue: &ExecutorTasksQueue) -> Result<usize> {
        match std::mem::replace(&mut self.task, ExecutorTask::None) {
            ExecutorTask::None => Err(ErrorCode::LogicalError("Execute none task.")),
            ExecutorTask::Sync(processor) => self.execute_sync_task(processor),
            ExecutorTask::Async(processor) => self.execute_async_task(processor, queue),
            ExecutorTask::AsyncSchedule(boxed_future) => self.schedule_async_task(boxed_future, queue),
        }
    }

    unsafe fn execute_sync_task(&mut self, processor: ProcessorPtr) -> Result<usize> {
        processor.process()?;
        Ok(0)
    }

    unsafe fn execute_async_task(&mut self, processor: ProcessorPtr, queue: &ExecutorTasksQueue) -> Result<usize> {
        let finished = Arc::new(AtomicBool::new(false));
        let mut future = processor.async_process();
        self.schedule_async_task(ExecutingAsyncTask { finished, future }, queue)
    }

    unsafe fn schedule_async_task(&mut self, mut task: ExecutingAsyncTask, queue: &ExecutorTasksQueue) -> Result<usize> {
        task.finished.store(false, Ordering::Relaxed);

        loop {
            let waker = ExecutingAsyncTaskWaker::create(&task.finished);

            let waker = futures::task::waker_ref(&waker);
            let mut cx = Context::from_waker(&waker);

            match task.future.as_mut().poll(&mut cx) {
                Poll::Ready(Ok(res)) => { return Ok(0); }
                Poll::Ready(Err(cause)) => { return Err(cause); }
                Poll::Pending => {
                    match queue.push_executing_async_task(self.worker_num, task) {
                        None => { return Ok(0); }
                        Some(t) => { task = t; }
                    };
                }
            };
        }
    }


    pub fn wait_wakeup(&self) {
        // condvar.wait(guard);
    }
}

struct ExecutingAsyncTaskWaker(Arc<AtomicBool>);

impl ExecutingAsyncTaskWaker {
    pub fn create(flag: &Arc<AtomicBool>) -> Arc<ExecutingAsyncTaskWaker> {
        Arc::new(ExecutingAsyncTaskWaker(flag.clone()))
    }
}

impl ArcWake for ExecutingAsyncTaskWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.store(true, Ordering::Release);
    }
}

