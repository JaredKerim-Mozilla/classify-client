use crate::{endpoints::EndpointState, errors::ClassifyError, APP_NAME};
use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use cadence::{prelude::*, BufferedUdpMetricSink, StatsdClient};
use futures::{future, Future, Poll};
use std::{
    fmt::Display,
    net::{ToSocketAddrs, UdpSocket},
    time::Instant,
};

pub fn get_client<A>(metrics_target: A, log: slog::Logger) -> Result<StatsdClient, ClassifyError>
where
    A: ToSocketAddrs + Display,
{
    let builder = {
        // Bind a socket to any/all interfaces (0.0.0.0) and an arbitrary
        // port, chosen by the OS (indicated by port 0). This port is used
        // only to send metrics data, and isn't used to receive anything.

        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_nonblocking(true)?;
        match BufferedUdpMetricSink::from(&metrics_target, socket) {
            Ok(udp_sink) => {
                let sink = cadence::QueuingMetricSink::from(udp_sink);
                StatsdClient::builder(APP_NAME, sink)
            }
            Err(err) => {
                slog::error!(
                    log,
                    "Could not connect to metrics host on {}: {}",
                    metrics_target,
                    err,
                );
                let sink = cadence::NopMetricSink;
                StatsdClient::builder(APP_NAME, sink)
            }
        }
    };
    Ok(builder
        .with_error_handler(move |error| slog::error!(log, "Could not send metric: {}", error))
        .build())
}

pub struct ResponseTimer;

impl<S, B> Transform<S> for ResponseTimer
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = ResponseTimerMiddleware<S>;
    type Future = future::FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(ResponseTimerMiddleware { service })
    }
}

pub struct ResponseTimerMiddleware<S> {
    service: S,
}

impl<S, B> Service for ResponseTimerMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        let metrics = match req.app_data::<EndpointState>() {
            Some(state) => state.metrics.clone(),
            None => return Box::new(self.service.call(req)),
        };
        let started = Instant::now();

        metrics.incr_with_tags("ongoing_requests").send();

        Box::new(self.service.call(req).and_then(move |res| {
            let duration = started.elapsed();
            metrics
                .time_duration_with_tags("response", duration)
                .with_tag(
                    "status",
                    if res.status().is_success() {
                        "success"
                    } else {
                        "error"
                    },
                )
                .send();
            metrics.decr_with_tags("ongoing_requests").send();
            Ok(res)
        }))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::endpoints::EndpointState;
    use actix_web::{
        test::{self, TestRequest},
        web, App, HttpResponse,
    };
    use cadence::StatsdClient;
    use regex::Regex;
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Debug)]
    pub struct TestMetricSink {
        pub log: Arc<Mutex<Vec<String>>>,
    }

    impl cadence::MetricSink for TestMetricSink {
        fn emit(&self, metric: &str) -> io::Result<usize> {
            let mut log = self.log.lock().unwrap();
            log.push(metric.to_owned());
            Ok(0)
        }
    }

    #[test]
    fn test_response_metrics_works() -> Result<(), Box<dyn std::error::Error>> {
        // Set up a service that logs metrics to vec we own
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = EndpointState {
            metrics: StatsdClient::from_sink("test", TestMetricSink { log: log.clone() }),
            ..EndpointState::default()
        };
        let mut service = test::init_service(App::new().data(state).wrap(ResponseTimer).route(
            "/",
            web::get().to(|| HttpResponse::InternalServerError().finish()),
        ));

        // Make a request to that service
        let request = TestRequest::with_uri("/").to_request();
        test::call_service(&mut service, request);

        // Check that the logged metric line looks as expected
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3, "three metrics should be logged");

        // two for the ongoing request increment and then decrement
        assert_eq!(log[0], "test.ongoing_requests:1|c");
        assert_eq!(log[2], "test.ongoing_requests:-1|c");

        // One for the overall status of the response
        let response_re = Regex::new(r"test.response:\d+|ms|#status:success")?;
        assert!(response_re.is_match(&log[1]));

        Ok(())
    }

    /// Test that if a request fails, an error is reported in metrics
    #[test]
    fn test_response_metrics_logs_error() -> Result<(), Box<dyn std::error::Error>> {
        // Set up a service that logs metrics to vec we own
        let log = Arc::new(Mutex::new(Vec::new()));
        let state = EndpointState {
            metrics: StatsdClient::from_sink("test", TestMetricSink { log: log.clone() }),
            ..EndpointState::default()
        };
        let mut service = test::init_service(App::new().data(state).wrap(ResponseTimer).route(
            "/",
            web::get().to(|| HttpResponse::InternalServerError().finish()),
        ));

        // Make a request to that service
        let request = TestRequest::with_uri("/").to_request();
        test::call_service(&mut service, request);

        // Check that the logged metric line looks as expected
        let log = log.lock().unwrap();
        let response_re = Regex::new(r"test.response:\d+|ms|#status:error")?;
        assert!(response_re.is_match(&log[1]));

        Ok(())
    }
}
