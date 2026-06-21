//! Capa AWS: `Env` + `AwsContext`/ClientFactory. Construye y cachea los clients
//! tipados por ambiente. Es uno de los pocos lugares (junto con `effects`) que
//! puede importar `aws-sdk-*`.

pub mod context;
