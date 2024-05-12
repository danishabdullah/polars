use std::fs::File;
use std::path::PathBuf;

use polars_core::export::arrow::Either;
use polars_core::POOL;
use polars_io::csv::read::{BatchedCsvReaderMmap, BatchedCsvReaderRead, CsvReadOptions, CsvReader};
use polars_plan::global::_set_n_rows_for_scan;
use polars_plan::prelude::FileScanOptions;
use polars_utils::iter::EnumerateIdxTrait;

use super::*;
use crate::pipeline::determine_chunk_size;

pub(crate) struct CsvSource {
    #[allow(dead_code)]
    // this exist because we need to keep ownership
    schema: SchemaRef,
    reader: Option<*mut CsvReader<File>>,
    batched_reader:
        Option<Either<*mut BatchedCsvReaderMmap<'static>, *mut BatchedCsvReaderRead<'static>>>,
    n_threads: usize,
    path: Option<PathBuf>,
    options: Option<CsvReadOptions>,
    file_options: Option<FileScanOptions>,
    verbose: bool,
}

impl CsvSource {
    // Delay initializing the reader
    // otherwise all files would be opened during construction of the pipeline
    // leading to Too many Open files error
    fn init_reader(&mut self) -> PolarsResult<()> {
        let options = self.options.take().unwrap();
        let file_options = self.file_options.take().unwrap();
        let path = self.path.take().unwrap();
        let mut with_columns = file_options.with_columns;
        let mut projected_len = 0;
        with_columns.as_ref().map(|columns| {
            projected_len = columns.len();
            columns
        });

        if projected_len == 0 {
            with_columns = None;
        }

        let n_cols = if projected_len > 0 {
            projected_len
        } else {
            self.schema.len()
        };
        let n_rows = _set_n_rows_for_scan(file_options.n_rows);
        // inversely scale the chunk size by the number of threads so that we reduce memory pressure
        // in streaming
        let chunk_size = determine_chunk_size(n_cols, POOL.current_num_threads())?;

        if self.verbose {
            eprintln!("STREAMING CHUNK SIZE: {chunk_size} rows")
        }

        let low_memory = options.low_memory;

        let reader: CsvReader<File> = options
            .with_skip_rows_after_header(
                // If we don't set it to 0 here, it will skip double the amount of rows.
                // But if we set it to 0, it will still skip the requested amount of rows.
                // TODO: Find out why. Maybe has something to do with schema inference.
                0,
            )
            .with_schema_overwrite(Some(self.schema.clone()))
            .with_n_rows(n_rows)
            .with_columns(with_columns)
            .with_rechunk(false)
            .with_row_index(file_options.row_index)
            .with_path(Some(path))
            .try_into_reader_with_file_path(None)?;

        let reader = Box::new(reader);
        let reader = Box::leak(reader) as *mut CsvReader<File>;

        let batched_reader = if low_memory {
            let batched_reader = unsafe { Box::new((*reader).batched_borrowed_read()?) };
            let batched_reader = Box::leak(batched_reader) as *mut BatchedCsvReaderRead;
            Either::Right(batched_reader)
        } else {
            let batched_reader = unsafe { Box::new((*reader).batched_borrowed_mmap()?) };
            let batched_reader = Box::leak(batched_reader) as *mut BatchedCsvReaderMmap;
            Either::Left(batched_reader)
        };
        self.reader = Some(reader);
        self.batched_reader = Some(batched_reader);
        Ok(())
    }

    pub(crate) fn new(
        path: PathBuf,
        schema: SchemaRef,
        options: CsvReadOptions,
        file_options: FileScanOptions,
        verbose: bool,
    ) -> PolarsResult<Self> {
        Ok(CsvSource {
            schema,
            reader: None,
            batched_reader: None,
            n_threads: POOL.current_num_threads(),
            path: Some(path),
            options: Some(options),
            file_options: Some(file_options),
            verbose,
        })
    }
}

impl Drop for CsvSource {
    fn drop(&mut self) {
        unsafe {
            match self.batched_reader {
                Some(Either::Left(ptr)) => {
                    let _to_drop = Box::from_raw(ptr);
                },
                Some(Either::Right(ptr)) => {
                    let _to_drop = Box::from_raw(ptr);
                },
                // nothing initialized, nothing to drop
                _ => {},
            }
            if let Some(ptr) = self.reader {
                let _to_drop = Box::from_raw(ptr);
            }
        };
    }
}

unsafe impl Send for CsvSource {}
unsafe impl Sync for CsvSource {}

impl Source for CsvSource {
    fn get_batches(&mut self, _context: &PExecutionContext) -> PolarsResult<SourceResult> {
        if self.reader.is_none() {
            self.init_reader()?
        }

        let batches = match self.batched_reader.unwrap() {
            Either::Left(batched_reader) => {
                let reader = unsafe { &mut *batched_reader };

                reader.next_batches(self.n_threads)?
            },
            Either::Right(batched_reader) => {
                let reader = unsafe { &mut *batched_reader };

                reader.next_batches(self.n_threads)?
            },
        };
        Ok(match batches {
            None => SourceResult::Finished,
            Some(batches) => {
                let index = get_source_index(0);
                let out = batches
                    .into_iter()
                    .enumerate_u32()
                    .map(|(i, data)| DataChunk {
                        chunk_index: (index + i) as IdxSize,
                        data,
                    })
                    .collect::<Vec<_>>();
                get_source_index(out.len() as u32);
                SourceResult::GotMoreData(out)
            },
        })
    }
    fn fmt(&self) -> &str {
        "csv"
    }
}
