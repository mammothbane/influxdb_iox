use test_helpers_end_to_end::{maybe_skip_integration, MiniCluster, Step, StepTest};

#[tokio::test]
async fn influxql_returns_error() {
    test_helpers::maybe_start_logging();
    let database_url = maybe_skip_integration!();

    let table_name = "the_table";

    // Set up the cluster  ====================================
    let mut cluster = MiniCluster::create_shared(database_url).await;

    StepTest::new(
        &mut cluster,
        vec![
            Step::WriteLineProtocol(format!(
                "{},tag1=A,tag2=B val=42i 123456\n\
                 {},tag1=A,tag2=C val=43i 123457",
                table_name, table_name
            )),
            Step::WaitForReadable,
            Step::AssertNotPersisted,
            Step::InfluxQLExpectingError {
                query: "SHOW TAG KEYS".into(),
                expected_error_code: tonic::Code::InvalidArgument,
                expected_message:
                    "Error while planning query: This feature is not implemented: SHOW TAG KEYS"
                        .into(),
            },
        ],
    )
    .run()
    .await
}

#[tokio::test]
async fn influxql_select_returns_results() {
    test_helpers::maybe_start_logging();
    let database_url = maybe_skip_integration!();

    let table_name = "the_table";

    // Set up the cluster  ====================================
    let mut cluster = MiniCluster::create_shared(database_url).await;

    StepTest::new(
        &mut cluster,
        vec![
            Step::WriteLineProtocol(format!(
                "{},tag1=A,tag2=B val=42i 123456\n\
                 {},tag1=A,tag2=C val=43i 123457",
                table_name, table_name
            )),
            Step::WaitForReadable,
            Step::AssertNotPersisted,
            Step::InfluxQLQuery {
                query: format!("select tag1, val from {}", table_name),
                expected: vec![
                    "+--------------------------------+------+-----+",
                    "| time                           | tag1 | val |",
                    "+--------------------------------+------+-----+",
                    "| 1970-01-01T00:00:00.000123456Z | A    | 42  |",
                    "| 1970-01-01T00:00:00.000123457Z | A    | 43  |",
                    "+--------------------------------+------+-----+",
                ],
            },
        ],
    )
    .run()
    .await
}
