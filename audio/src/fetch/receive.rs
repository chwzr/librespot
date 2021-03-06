use std::cmp::{max, min};
use std::io::{Seek, SeekFrom, Write};
use std::sync::{atomic, Arc};
use std::time::Instant;

use byteorder::{BigEndian, WriteBytesExt};
use bytes::Bytes;
use futures_util::StreamExt;
use librespot_core::channel::{Channel, ChannelData};
use librespot_core::session::Session;
use librespot_core::spotify_id::FileId;
use tempfile::NamedTempFile;
use tokio::sync::{mpsc, oneshot};

use crate::range_set::{Range, RangeSet};

use super::{AudioFileShared, DownloadStrategy, StreamLoaderCommand};
use super::{
    FAST_PREFETCH_THRESHOLD_FACTOR, MAXIMUM_ASSUMED_PING_TIME_SECONDS, MAX_PREFETCH_REQUESTS,
    MINIMUM_DOWNLOAD_SIZE, PREFETCH_THRESHOLD_FACTOR,
};

pub fn request_range(session: &Session, file: FileId, offset: usize, length: usize) -> Channel {
    assert!(
        offset % 4 == 0,
        "Range request start positions must be aligned by 4 bytes."
    );
    assert!(
        length % 4 == 0,
        "Range request range lengths must be aligned by 4 bytes."
    );
    let start = offset / 4;
    let end = (offset + length) / 4;

    let (id, channel) = session.channel().allocate();

    let mut data: Vec<u8> = Vec::new();
    data.write_u16::<BigEndian>(id).unwrap();
    data.write_u8(0).unwrap();
    data.write_u8(1).unwrap();
    data.write_u16::<BigEndian>(0x0000).unwrap();
    data.write_u32::<BigEndian>(0x00000000).unwrap();
    data.write_u32::<BigEndian>(0x00009C40).unwrap();
    data.write_u32::<BigEndian>(0x00020000).unwrap();
    data.write(&file.0).unwrap();
    data.write_u32::<BigEndian>(start as u32).unwrap();
    data.write_u32::<BigEndian>(end as u32).unwrap();

    session.send_packet(0x8, data);

    channel
}

struct PartialFileData {
    offset: usize,
    data: Bytes,
}

enum ReceivedData {
    ResponseTimeMs(usize),
    Data(PartialFileData),
}

async fn receive_data(
    shared: Arc<AudioFileShared>,
    file_data_tx: mpsc::UnboundedSender<ReceivedData>,
    mut data_rx: ChannelData,
    initial_data_offset: usize,
    initial_request_length: usize,
    request_sent_time: Instant,
    mut measure_ping_time: bool,
    finish_tx: mpsc::UnboundedSender<()>,
) {
    let mut data_offset = initial_data_offset;
    let mut request_length = initial_request_length;

    let result = loop {
        let data = match data_rx.next().await {
            Some(Ok(data)) => data,
            Some(Err(e)) => break Err(e),
            None => break Ok(()),
        };

        if measure_ping_time {
            let duration = Instant::now() - request_sent_time;
            let duration_ms: u64;
            if 0.001 * (duration.as_millis() as f64) > MAXIMUM_ASSUMED_PING_TIME_SECONDS {
                duration_ms = (MAXIMUM_ASSUMED_PING_TIME_SECONDS * 1000.0) as u64;
            } else {
                duration_ms = duration.as_millis() as u64;
            }
            let _ = file_data_tx.send(ReceivedData::ResponseTimeMs(duration_ms as usize));
            measure_ping_time = false;
        }
        let data_size = data.len();
        let _ = file_data_tx.send(ReceivedData::Data(PartialFileData {
            offset: data_offset,
            data,
        }));
        data_offset += data_size;
        if request_length < data_size {
            warn!(
                "Data receiver for range {} (+{}) received more data from server than requested.",
                initial_data_offset, initial_request_length
            );
            request_length = 0;
        } else {
            request_length -= data_size;
        }

        if request_length == 0 {
            break Ok(());
        }
    };

    if request_length > 0 {
        let missing_range = Range::new(data_offset, request_length);

        let mut download_status = shared.download_status.lock().unwrap();
        download_status.requested.subtract_range(&missing_range);
        shared.cond.notify_all();
    }

    let _ = finish_tx.send(());

    if result.is_err() {
        warn!(
            "Error from channel for data receiver for range {} (+{}).",
            initial_data_offset, initial_request_length
        );
    } else if request_length > 0 {
        warn!(
            "Data receiver for range {} (+{}) received less data from server than requested.",
            initial_data_offset, initial_request_length
        );
    }
}

struct AudioFileFetch {
    session: Session,
    shared: Arc<AudioFileShared>,
    output: Option<NamedTempFile>,

    file_data_tx: mpsc::UnboundedSender<ReceivedData>,
    complete_tx: Option<oneshot::Sender<NamedTempFile>>,
    network_response_times_ms: Vec<usize>,
    number_of_open_requests: usize,

    download_finish_tx: mpsc::UnboundedSender<()>,
}

// Might be replaced by enum from std once stable
#[derive(PartialEq, Eq)]
enum ControlFlow {
    Break,
    Continue,
}

impl AudioFileFetch {
    fn get_download_strategy(&mut self) -> DownloadStrategy {
        *(self.shared.download_strategy.lock().unwrap())
    }

    fn download_range(&mut self, mut offset: usize, mut length: usize) {
        if length < MINIMUM_DOWNLOAD_SIZE {
            length = MINIMUM_DOWNLOAD_SIZE;
        }

        // ensure the values are within the bounds and align them by 4 for the spotify protocol.
        if offset >= self.shared.file_size {
            return;
        }

        if length == 0 {
            return;
        }

        if offset + length > self.shared.file_size {
            length = self.shared.file_size - offset;
        }

        if offset % 4 != 0 {
            length += offset % 4;
            offset -= offset % 4;
        }

        if length % 4 != 0 {
            length += 4 - (length % 4);
        }

        let mut ranges_to_request = RangeSet::new();
        ranges_to_request.add_range(&Range::new(offset, length));

        let mut download_status = self.shared.download_status.lock().unwrap();

        ranges_to_request.subtract_range_set(&download_status.downloaded);
        ranges_to_request.subtract_range_set(&download_status.requested);

        for range in ranges_to_request.iter() {
            let (_headers, data) = request_range(
                &self.session,
                self.shared.file_id,
                range.start,
                range.length,
            )
            .split();

            download_status.requested.add_range(range);

            self.session.spawn(receive_data(
                self.shared.clone(),
                self.file_data_tx.clone(),
                data,
                range.start,
                range.length,
                Instant::now(),
                self.number_of_open_requests == 0,
                self.download_finish_tx.clone(),
            ));

            self.number_of_open_requests += 1;
        }
    }

    fn pre_fetch_more_data(&mut self, bytes: usize, max_requests_to_send: usize) {
        let mut bytes_to_go = bytes;
        let mut requests_to_go = max_requests_to_send;

        while bytes_to_go > 0 && requests_to_go > 0 {
            // determine what is still missing
            let mut missing_data = RangeSet::new();
            missing_data.add_range(&Range::new(0, self.shared.file_size));
            {
                let download_status = self.shared.download_status.lock().unwrap();
                missing_data.subtract_range_set(&download_status.downloaded);
                missing_data.subtract_range_set(&download_status.requested);
            }

            // download data from after the current read position first
            let mut tail_end = RangeSet::new();
            let read_position = self.shared.read_position.load(atomic::Ordering::Relaxed);
            tail_end.add_range(&Range::new(
                read_position,
                self.shared.file_size - read_position,
            ));
            let tail_end = tail_end.intersection(&missing_data);

            if !tail_end.is_empty() {
                let range = tail_end.get_range(0);
                let offset = range.start;
                let length = min(range.length, bytes_to_go);
                self.download_range(offset, length);
                requests_to_go -= 1;
                bytes_to_go -= length;
            } else if !missing_data.is_empty() {
                // ok, the tail is downloaded, download something fom the beginning.
                let range = missing_data.get_range(0);
                let offset = range.start;
                let length = min(range.length, bytes_to_go);
                self.download_range(offset, length);
                requests_to_go -= 1;
                bytes_to_go -= length;
            } else {
                return;
            }
        }
    }

    fn handle_file_data(&mut self, data: ReceivedData) -> ControlFlow {
        match data {
            ReceivedData::ResponseTimeMs(response_time_ms) => {
                trace!("Ping time estimated as: {} ms.", response_time_ms);

                // record the response time
                self.network_response_times_ms.push(response_time_ms);

                // prune old response times. Keep at most three.
                while self.network_response_times_ms.len() > 3 {
                    self.network_response_times_ms.remove(0);
                }

                // stats::median is experimental. So we calculate the median of up to three ourselves.
                let ping_time_ms: usize = match self.network_response_times_ms.len() {
                    1 => self.network_response_times_ms[0] as usize,
                    2 => {
                        ((self.network_response_times_ms[0] + self.network_response_times_ms[1])
                            / 2) as usize
                    }
                    3 => {
                        let mut times = self.network_response_times_ms.clone();
                        times.sort_unstable();
                        times[1]
                    }
                    _ => unreachable!(),
                };

                // store our new estimate for everyone to see
                self.shared
                    .ping_time_ms
                    .store(ping_time_ms, atomic::Ordering::Relaxed);
            }
            ReceivedData::Data(data) => {
                self.output
                    .as_mut()
                    .unwrap()
                    .seek(SeekFrom::Start(data.offset as u64))
                    .unwrap();
                self.output
                    .as_mut()
                    .unwrap()
                    .write_all(data.data.as_ref())
                    .unwrap();

                let mut download_status = self.shared.download_status.lock().unwrap();

                let received_range = Range::new(data.offset, data.data.len());
                download_status.downloaded.add_range(&received_range);
                self.shared.cond.notify_all();

                let full = download_status.downloaded.contained_length_from_value(0)
                    >= self.shared.file_size;

                drop(download_status);

                if full {
                    self.finish();
                    return ControlFlow::Break;
                }
            }
        }
        ControlFlow::Continue
    }

    fn handle_stream_loader_command(&mut self, cmd: StreamLoaderCommand) -> ControlFlow {
        match cmd {
            StreamLoaderCommand::Fetch(request) => {
                self.download_range(request.start, request.length);
            }
            StreamLoaderCommand::RandomAccessMode() => {
                *(self.shared.download_strategy.lock().unwrap()) = DownloadStrategy::RandomAccess();
            }
            StreamLoaderCommand::StreamMode() => {
                *(self.shared.download_strategy.lock().unwrap()) = DownloadStrategy::Streaming();
                self.trigger_preload();
            }
            StreamLoaderCommand::Close() => return ControlFlow::Break,
        }
        ControlFlow::Continue
    }

    fn finish(&mut self) {
        let mut output = self.output.take().unwrap();
        let complete_tx = self.complete_tx.take().unwrap();

        output.seek(SeekFrom::Start(0)).unwrap();
        let _ = complete_tx.send(output);
    }

    fn trigger_preload(&mut self) {
        if self.number_of_open_requests >= MAX_PREFETCH_REQUESTS {
            return;
        }

        let max_requests_to_send = MAX_PREFETCH_REQUESTS - self.number_of_open_requests;

        let bytes_pending: usize = {
            let download_status = self.shared.download_status.lock().unwrap();
            download_status
                .requested
                .minus(&download_status.downloaded)
                .len()
        };

        let ping_time_seconds =
            0.001 * self.shared.ping_time_ms.load(atomic::Ordering::Relaxed) as f64;
        let download_rate = self.session.channel().get_download_rate_estimate();

        let desired_pending_bytes = max(
            (PREFETCH_THRESHOLD_FACTOR * ping_time_seconds * self.shared.stream_data_rate as f64)
                as usize,
            (FAST_PREFETCH_THRESHOLD_FACTOR * ping_time_seconds * download_rate as f64) as usize,
        );

        if bytes_pending < desired_pending_bytes {
            self.pre_fetch_more_data(desired_pending_bytes - bytes_pending, max_requests_to_send);
        }
    }
}

pub(super) async fn audio_file_fetch(
    session: Session,
    shared: Arc<AudioFileShared>,
    initial_data_rx: ChannelData,
    initial_request_sent_time: Instant,
    initial_data_length: usize,

    output: NamedTempFile,
    mut stream_loader_command_rx: mpsc::UnboundedReceiver<StreamLoaderCommand>,
    complete_tx: oneshot::Sender<NamedTempFile>,
) {
    let (file_data_tx, mut file_data_rx) = mpsc::unbounded_channel();
    let (download_finish_tx, mut download_finish_rx) = mpsc::unbounded_channel();

    {
        let requested_range = Range::new(0, initial_data_length);
        let mut download_status = shared.download_status.lock().unwrap();
        download_status.requested.add_range(&requested_range);
    }

    session.spawn(receive_data(
        shared.clone(),
        file_data_tx.clone(),
        initial_data_rx,
        0,
        initial_data_length,
        initial_request_sent_time,
        true,
        download_finish_tx.clone(),
    ));

    let mut fetch = AudioFileFetch {
        session,
        shared,
        output: Some(output),

        file_data_tx,
        complete_tx: Some(complete_tx),
        network_response_times_ms: Vec::new(),
        number_of_open_requests: 1,

        download_finish_tx,
    };

    loop {
        tokio::select! {
            cmd = stream_loader_command_rx.recv() => {
                if cmd.map_or(true, |cmd| fetch.handle_stream_loader_command(cmd) == ControlFlow::Break) {
                    break;
                }
            },
            data = file_data_rx.recv() => {
                if data.map_or(true, |data| fetch.handle_file_data(data) == ControlFlow::Break) {
                    break;
                }
            },
            _ = download_finish_rx.recv() => {
                fetch.number_of_open_requests -= 1;

                if fetch.get_download_strategy() == DownloadStrategy::Streaming() {
                    fetch.trigger_preload();
                }
            }
        }
    }
}
