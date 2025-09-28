pub mod schema {
    include!(concat!(env!("OUT_DIR"), "/schema.schema.rs"));
}

pub mod common {
    include!(concat!(env!("OUT_DIR"), "/schema.common.rs"));
}

pub mod sink {
    include!(concat!(env!("OUT_DIR"), "/schema.sink.rs"));
}

pub mod source {
    include!(concat!(env!("OUT_DIR"), "/schema.source.rs"));
}

pub mod transform {
    include!(concat!(env!("OUT_DIR"), "/schema.transform.rs"));
}
