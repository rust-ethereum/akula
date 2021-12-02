use super::{
    fetch_receive_stage::FetchReceiveStage, fetch_request_stage::FetchRequestStage, header_slices,
    header_slices::HeaderSlices, penalize_stage::PenalizeStage, refill_stage::RefillStage,
    retry_stage::RetryStage, save_stage::SaveStage,
    top_block_estimate_stage::TopBlockEstimateStage, verify_stage_linear::VerifyStageLinear,
    verify_stage_linear_link::VerifyStageLinearLink, HeaderSlicesView,
};
use crate::{
    downloader::{
        headers::stage_stream::{make_stage_stream, StageStream},
        ui_system::UISystem,
    },
    kv,
    models::BlockNumber,
    sentry::{chain_config::ChainConfig, messages::BlockHashAndNumber, sentry_client_reactor::*},
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::{StreamExt, StreamMap};
use tracing::*;

pub struct DownloaderLinear {
    chain_config: ChainConfig,
    mem_limit: usize,
    sentry: SentryClientReactorShared,
    ui_system: Arc<Mutex<UISystem>>,
}

impl DownloaderLinear {
    pub fn new(
        chain_config: ChainConfig,
        mem_limit: usize,
        sentry: SentryClientReactorShared,
        ui_system: Arc<Mutex<UISystem>>,
    ) -> Self {
        Self {
            chain_config,
            mem_limit,
            sentry,
            ui_system,
        }
    }

    async fn estimate_top_block_num(
        &self,
        start_block_num: BlockNumber,
    ) -> anyhow::Result<BlockNumber> {
        info!("DownloaderLinear: waiting to estimate a top block number...");
        let stage = TopBlockEstimateStage::new(self.sentry.clone());
        while !stage.is_over() && stage.estimated_top_block_num().is_none() {
            stage.execute().await?;
        }
        let estimated_top_block_num = stage.estimated_top_block_num().unwrap_or(start_block_num);
        info!(
            "DownloaderLinear: estimated top block number = {}",
            estimated_top_block_num.0
        );
        Ok(estimated_top_block_num)
    }

    pub async fn run<'downloader, 'db: 'downloader, RwTx: kv::traits::MutableTransaction<'db>>(
        &'downloader self,
        db_transaction: &'downloader RwTx,
        start_block_id: BlockHashAndNumber,
        estimated_top_block_num: Option<BlockNumber>,
    ) -> anyhow::Result<BlockNumber> {
        let start_block_num = start_block_id.number;

        let trusted_len: u64 = 90_000;

        let estimated_top_block_num = match estimated_top_block_num {
            Some(block_num) => block_num.0,
            None => self.estimate_top_block_num(start_block_num).await?.0,
        };

        let slice_size = header_slices::HEADER_SLICE_SIZE as u64;
        let final_block_num = if estimated_top_block_num > trusted_len {
            (estimated_top_block_num - trusted_len) / slice_size * slice_size
        } else {
            0
        };
        let final_block_num = BlockNumber(final_block_num);

        if start_block_num.0 >= final_block_num.0 {
            return Ok(start_block_num);
        }

        let header_slices = Arc::new(HeaderSlices::new(
            self.mem_limit,
            start_block_num,
            final_block_num,
        ));
        let sentry = self.sentry.clone();

        let header_slices_view = HeaderSlicesView::new(header_slices.clone(), "DownloaderLinear");
        self.ui_system
            .try_lock()?
            .set_view(Some(Box::new(header_slices_view)));

        // Downloading happens with several stages where
        // each of the stages processes blocks in one status,
        // and updates them to proceed to the next status.
        // All stages runs in parallel,
        // although most of the time only one of the stages is actively running,
        // while the others are waiting for the status updates or timeouts.

        let fetch_request_stage = FetchRequestStage::new(
            header_slices.clone(),
            sentry.clone(),
            header_slices::HEADER_SLICE_SIZE,
        );
        let fetch_receive_stage = FetchReceiveStage::new(header_slices.clone(), sentry.clone());
        let retry_stage = RetryStage::new(header_slices.clone());
        let verify_stage = VerifyStageLinear::new(
            header_slices.clone(),
            header_slices::HEADER_SLICE_SIZE,
            self.chain_config.clone(),
        );
        let verify_link_stage = VerifyStageLinearLink::new(
            header_slices.clone(),
            self.chain_config.clone(),
            start_block_num,
            start_block_id.hash,
        );
        let penalize_stage = PenalizeStage::new(header_slices.clone(), sentry.clone());
        let save_stage = SaveStage::<RwTx>::new(header_slices.clone(), db_transaction);
        let refill_stage = RefillStage::new(header_slices.clone());

        let can_proceed = fetch_receive_stage.can_proceed_check();

        let mut stream = StreamMap::<&str, StageStream>::new();
        stream.insert(
            "fetch_request_stage",
            make_stage_stream(fetch_request_stage),
        );
        stream.insert(
            "fetch_receive_stage",
            make_stage_stream(fetch_receive_stage),
        );
        stream.insert("retry_stage", make_stage_stream(retry_stage));
        stream.insert("verify_stage", make_stage_stream(verify_stage));
        stream.insert("verify_link_stage", make_stage_stream(verify_link_stage));
        stream.insert("penalize_stage", make_stage_stream(penalize_stage));
        stream.insert("save_stage", make_stage_stream(save_stage));
        stream.insert("refill_stage", make_stage_stream(refill_stage));

        while let Some((key, result)) = stream.next().await {
            if result.is_err() {
                error!("Downloader headers {} failure: {:?}", key, result);
                break;
            }

            if !can_proceed() {
                break;
            }
            if header_slices.is_empty_at_final_position() {
                break;
            }

            header_slices.notify_status_watchers();
        }

        Ok(header_slices.min_block_num())
    }
}
