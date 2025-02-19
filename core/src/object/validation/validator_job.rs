use crate::{
	extract_job_data,
	job::{
		JobError, JobInitData, JobReportUpdate, JobResult, JobState, StatefulJob, WorkerContext,
	},
	library::Library,
	location::file_path_helper::{
		ensure_file_path_exists, ensure_sub_path_is_directory, ensure_sub_path_is_in_location,
		file_path_for_object_validator, IsolatedFilePathData,
	},
	prisma::{file_path, location},
	sync,
	util::{
		db::{chain_optional_iter, maybe_missing},
		error::FileIOError,
	},
};

use std::{
	hash::{Hash, Hasher},
	path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use super::{hash::file_checksum, ValidatorError};

// The Validator is able to:
// - generate a full byte checksum for Objects in a Location
// - generate checksums for all Objects missing without one
// - compare two objects and return true if they are the same
pub struct ObjectValidatorJob {}

#[derive(Serialize, Deserialize, Debug)]
pub struct ObjectValidatorJobState {
	pub location_path: PathBuf,
	pub task_count: usize,
}

// The validator can
#[derive(Serialize, Deserialize, Debug)]
pub struct ObjectValidatorJobInit {
	pub location: location::Data,
	pub sub_path: Option<PathBuf>,
}

impl Hash for ObjectValidatorJobInit {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.location.id.hash(state);
		if let Some(ref sub_path) = self.sub_path {
			sub_path.hash(state);
		}
	}
}

impl JobInitData for ObjectValidatorJobInit {
	type Job = ObjectValidatorJob;
}

#[async_trait::async_trait]
impl StatefulJob for ObjectValidatorJob {
	type Init = ObjectValidatorJobInit;
	type Data = ObjectValidatorJobState;
	type Step = file_path_for_object_validator::Data;

	const NAME: &'static str = "object_validator";

	fn new() -> Self {
		Self {}
	}

	async fn init(
		&self,
		ctx: &mut WorkerContext,
		state: &mut JobState<Self>,
	) -> Result<(), JobError> {
		let Library { db, .. } = &ctx.library;

		let location_id = state.init.location.id;

		let location_path =
			maybe_missing(&state.init.location.path, "location.path").map(PathBuf::from)?;

		let maybe_sub_iso_file_path = match &state.init.sub_path {
			Some(sub_path) if sub_path != Path::new("") && sub_path != Path::new("/") => {
				let full_path = ensure_sub_path_is_in_location(&location_path, sub_path)
					.await
					.map_err(ValidatorError::from)?;
				ensure_sub_path_is_directory(&location_path, sub_path)
					.await
					.map_err(ValidatorError::from)?;

				let sub_iso_file_path =
					IsolatedFilePathData::new(location_id, &location_path, &full_path, true)
						.map_err(ValidatorError::from)?;

				ensure_file_path_exists(
					sub_path,
					&sub_iso_file_path,
					db,
					ValidatorError::SubPathNotFound,
				)
				.await?;

				Some(sub_iso_file_path)
			}
			_ => None,
		};

		state.steps.extend(
			db.file_path()
				.find_many(chain_optional_iter(
					[
						file_path::location_id::equals(Some(state.init.location.id)),
						file_path::is_dir::equals(Some(false)),
						file_path::integrity_checksum::equals(None),
					],
					[maybe_sub_iso_file_path.and_then(|iso_sub_path| {
						iso_sub_path
							.materialized_path_for_children()
							.map(file_path::materialized_path::starts_with)
					})],
				))
				.select(file_path_for_object_validator::select())
				.exec()
				.await?,
		);

		state.data = Some(ObjectValidatorJobState {
			location_path,
			task_count: state.steps.len(),
		});

		ctx.progress(vec![JobReportUpdate::TaskCount(state.steps.len())]);

		Ok(())
	}

	async fn execute_step(
		&self,
		ctx: &mut WorkerContext,
		state: &mut JobState<Self>,
	) -> Result<(), JobError> {
		let Library { db, sync, .. } = &ctx.library;

		let file_path = &state.steps[0];
		let data = extract_job_data!(state);

		// this is to skip files that already have checksums
		// i'm unsure what the desired behaviour is in this case
		// we can also compare old and new checksums here
		// This if is just to make sure, we already queried objects where integrity_checksum is null
		if file_path.integrity_checksum.is_none() {
			let full_path = data.location_path.join(IsolatedFilePathData::try_from((
				state.init.location.id,
				file_path,
			))?);
			let checksum = file_checksum(&full_path)
				.await
				.map_err(|e| ValidatorError::FileIO(FileIOError::from((full_path, e))))?;

			sync.write_op(
				db,
				sync.shared_update(
					sync::file_path::SyncId {
						pub_id: file_path.pub_id.clone(),
					},
					file_path::integrity_checksum::NAME,
					json!(&checksum),
				),
				db.file_path().update(
					file_path::pub_id::equals(file_path.pub_id.clone()),
					vec![file_path::integrity_checksum::set(Some(checksum))],
				),
			)
			.await?;
		}

		ctx.progress(vec![JobReportUpdate::CompletedTaskCount(
			state.step_number + 1,
		)]);

		Ok(())
	}

	async fn finalize(
		&mut self,
		_ctx: &mut WorkerContext,
		state: &mut JobState<Self>,
	) -> JobResult {
		let data = extract_job_data!(state);
		info!(
			"finalizing validator job at {}{}: {} tasks",
			data.location_path.display(),
			state
				.init
				.sub_path
				.as_ref()
				.map(|p| format!("{}", p.display()))
				.unwrap_or_default(),
			data.task_count
		);

		Ok(Some(serde_json::to_value(&state.init)?))
	}
}
