// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Benchmark for evaluating row filters and projections on a Parquet file.
//!
//! This benchmark creates a Parquet file in memory with 100K rows and four columns:
//!  - int64: sequential integers
//!  - float64: floating-point values (derived from the integers)
//!  - utf8View: string values where about half are non-empty,
//!    and a few rows (every 10Kth row) are the constant "const"
//!  - ts: timestamp values (using, e.g., a millisecond epoch)
//!
//! It then applies several filter functions and projections, benchmarking the read-back speed.
//!
//! Filters tested:
//!  - A string filter: `utf8View <> ''` (non-empty)
//!  - A string filter: `utf8View = 'const'` (selective)
//!  - An integer non-selective filter (e.g. even numbers)
//!  - An integer selective filter (e.g. `int64 = 0`)
//!  - A timestamp filter (e.g. `ts > threshold`)
//!
//! Projections tested:
//!  - All 4 columns.
//!  - All columns except the one used for the filter.
//!
//! To run the benchmark, use `cargo bench --bench bench_filter_projection`.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::sync::Arc;
use tempfile::NamedTempFile;

use arrow::array::{
    ArrayRef, BooleanArray, BooleanBuilder, Float64Array, Int64Array, TimestampMillisecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use arrow_array::builder::StringViewBuilder;
use arrow_array::{Array, StringViewArray};
use parquet::arrow::arrow_reader::{
    ArrowPredicateFn, ArrowReaderBuilder, ArrowReaderOptions, RowFilter,
};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::file::properties::WriterProperties;

/// Create a RecordBatch with 100K rows and four columns.
fn make_record_batch() -> RecordBatch {
    let num_rows = 100_000;

    // int64 column: sequential numbers 0..num_rows
    let int_values: Vec<i64> = (0..num_rows as i64).collect();
    let int_array = Arc::new(Int64Array::from(int_values)) as ArrayRef;

    // float64 column: derived from int64 (e.g., multiplied by 0.1)
    let float_values: Vec<f64> = (0..num_rows).map(|i| i as f64 * 0.1).collect();
    let float_array = Arc::new(Float64Array::from(float_values)) as ArrayRef;

    // utf8View column: even rows get non-empty strings; odd rows get an empty string;
    // every 10Kth even row is "const" to be selective.
    let mut string_view_builder = StringViewBuilder::with_capacity(100_000);
    for i in 0..num_rows {
        if i % 2 == 0 {
            if i % 10_000 == 0 {
                string_view_builder.append_value("const");
            } else {
                string_view_builder.append_value("nonempty");
            }
        } else {
            string_view_builder.append_value("");
        }
    }
    let utf8_view_array = Arc::new(string_view_builder.finish()) as ArrayRef;

    // Timestamp column: using milliseconds from an epoch (simply using the row index)
    let ts_values: Vec<i64> = (0..num_rows as i64).collect();
    let ts_array = Arc::new(TimestampMillisecondArray::from(ts_values)) as ArrayRef;

    let schema = Arc::new(Schema::new(vec![
        Field::new("int64", DataType::Int64, false),
        Field::new("float64", DataType::Float64, false),
        Field::new("utf8View", DataType::Utf8View, false),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Millisecond, None),
            false,
        ),
    ]));

    RecordBatch::try_new(
        schema,
        vec![int_array, float_array, utf8_view_array, ts_array],
    )
    .unwrap()
}

/// Writes the record batch to a temporary Parquet file.
fn write_parquet_file() -> NamedTempFile {
    let batch = make_record_batch();
    let schema = batch.schema();
    let props = WriterProperties::builder().build();

    let file = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .unwrap();
    {
        let file_reopen = file.reopen().unwrap();
        let mut writer = ArrowWriter::try_new(file_reopen, schema.clone(), Some(props)).unwrap();
        // Write the entire batch as a single row group.
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }
    file
}

/// Filter function: returns a BooleanArray with true when utf8View <> "".
fn filter_utf8_view_nonempty(batch: &RecordBatch) -> BooleanArray {
    let array = batch
        .column(batch.schema().index_of("utf8View").unwrap())
        .as_any()
        .downcast_ref::<StringViewArray>()
        .unwrap();
    let mut builder = BooleanBuilder::with_capacity(array.len());
    for i in 0..array.len() {
        let keep = !array.value(i).is_empty();
        builder.append_value(keep);
    }
    builder.finish()
}

/// Filter function: returns a BooleanArray with true when utf8View == "const".
fn filter_utf8_view_const(batch: &RecordBatch) -> BooleanArray {
    let array = batch
        .column(batch.schema().index_of("utf8View").unwrap())
        .as_any()
        .downcast_ref::<StringViewArray>()
        .unwrap();
    let mut builder = BooleanBuilder::with_capacity(array.len());
    for i in 0..array.len() {
        let keep = array.value(i) == "const";
        builder.append_value(keep);
    }
    builder.finish()
}

/// Integer non-selective filter: returns true for even numbers.
fn filter_int64_even(batch: &RecordBatch) -> BooleanArray {
    let array = batch
        .column(batch.schema().index_of("int64").unwrap())
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let mut builder = BooleanBuilder::with_capacity(array.len());
    for i in 0..array.len() {
        let keep = array.value(i) % 2 == 0;
        builder.append_value(keep);
    }
    builder.finish()
}

/// Integer selective filter: returns true only when int64 equals 0.
fn filter_int64_eq_zero(batch: &RecordBatch) -> BooleanArray {
    let array = batch
        .column(batch.schema().index_of("int64").unwrap())
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    let mut builder = BooleanBuilder::with_capacity(array.len());
    for i in 0..array.len() {
        let keep = array.value(i) == 0;
        builder.append_value(keep);
    }
    builder.finish()
}

/// Timestamp filter: returns true when ts > threshold (using 50_000 as example threshold).
fn filter_timestamp_gt(batch: &RecordBatch) -> BooleanArray {
    let array = batch
        .column(batch.schema().index_of("ts").unwrap())
        .as_any()
        .downcast_ref::<TimestampMillisecondArray>()
        .unwrap();
    let threshold = 50_000;
    let mut builder = BooleanBuilder::with_capacity(array.len());
    for i in 0..array.len() {
        let keep = array.value(i) > threshold;
        builder.append_value(keep);
    }
    builder.finish()
}

#[derive(Clone)]
enum FilterType {
    Utf8ViewNonEmpty,
    Utf8ViewConst,
    Int64Even,
    Int64EqZero,
    TimestampGt,
}

impl std::fmt::Display for FilterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterType::Utf8ViewNonEmpty => write!(f, "utf8View <> ''"),
            FilterType::Utf8ViewConst => write!(f, "utf8View = 'const'"),
            FilterType::Int64Even => write!(f, "int64 even"),
            FilterType::Int64EqZero => write!(f, "int64 = 0"),
            FilterType::TimestampGt => write!(f, "ts > 50_000"),
        }
    }
}

fn benchmark_filters_and_projections(c: &mut Criterion) {
    let parquet_file = write_parquet_file();

    // Define filter functions associated with each FilterType.
    type FilterFn = fn(&RecordBatch) -> BooleanArray;
    let filter_funcs: Vec<(FilterType, FilterFn)> = vec![
        (FilterType::Utf8ViewNonEmpty, filter_utf8_view_nonempty),
        (FilterType::Utf8ViewConst, filter_utf8_view_const),
        (FilterType::Int64Even, filter_int64_even),
        (FilterType::Int64EqZero, filter_int64_eq_zero),
        (FilterType::TimestampGt, filter_timestamp_gt),
    ];

    let mut group = c.benchmark_group("arrow_reader_row_filter");

    // Iterate by value (Copy is available for FilterType and fn pointers)
    for (filter_type, filter_fn) in filter_funcs.into_iter() {
        for proj_case in ["all_columns", "exclude_filter_column"].iter() {
            // Define indices for all columns: [0: "int64", 1: "float64", 2: "utf8View", 3: "ts"]
            let all_indices = vec![0, 1, 2, 3];

            // For the output projection, conditionally exclude the filter column.
            let output_projection: Vec<usize> = if *proj_case == "all_columns" {
                all_indices.clone()
            } else {
                all_indices
                    .into_iter()
                    .filter(|i| match filter_type {
                        FilterType::Utf8ViewNonEmpty | FilterType::Utf8ViewConst => *i != 2, // Exclude "utf8" (index 2)
                        FilterType::Int64Even | FilterType::Int64EqZero => *i != 0, // Exclude "int64" (index 0)
                        FilterType::TimestampGt => *i != 3, // Exclude "ts" (index 3)
                    })
                    .collect()
            };

            // For predicate pushdown, define a projection that includes the column required for the predicate.
            let predicate_projection: Vec<usize> = match filter_type {
                FilterType::Utf8ViewNonEmpty | FilterType::Utf8ViewConst => vec![2],
                FilterType::Int64Even | FilterType::Int64EqZero => vec![0],
                FilterType::TimestampGt => vec![3],
            };

            // Create a benchmark id combining filter type and projection case.
            let bench_id = BenchmarkId::new(
                format!("filter_case: {} project_case: {}", filter_type, proj_case),
                "",
            );
            group.bench_function(bench_id, |b| {
                b.iter(|| {
                    // Reopen the Parquet file for each iteration.
                    let file = parquet_file.reopen().unwrap();
                    let options = ArrowReaderOptions::new().with_page_index(true);
                    let builder = ArrowReaderBuilder::try_new_with_options(file, options).unwrap();
                    let file_metadata = builder.metadata().file_metadata().clone();
                    // Build the projection mask from the output projection (clone to avoid move)
                    let mask = ProjectionMask::roots(
                        file_metadata.schema_descr(),
                        output_projection.clone(),
                    );

                    // Build the predicate mask from the predicate projection (clone to avoid move)
                    let pred_mask = ProjectionMask::roots(
                        file_metadata.schema_descr(),
                        predicate_projection.clone(),
                    );

                    // Copy the filter function pointer.
                    let f = filter_fn;
                    // Wrap the filter function in a closure to satisfy the expected signature.
                    let filter =
                        ArrowPredicateFn::new(pred_mask, move |batch: RecordBatch| Ok(f(&batch)));
                    let row_filter = RowFilter::new(vec![Box::new(filter)]);

                    // Build the reader with row filter and output projection.
                    let reader = builder
                        .with_row_filter(row_filter)
                        .with_projection(mask)
                        .build()
                        .unwrap();

                    // Collect result batches, unwrapping errors.
                    let _result: Vec<RecordBatch> = reader.map(|r| r.unwrap()).collect();
                });
            });
        }
    }

    group.finish();
}

criterion_group!(benches, benchmark_filters_and_projections);
criterion_main!(benches);
