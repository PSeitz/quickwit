//  Quickwit
//  Copyright (C) 2021 Quickwit Inc.
//
//  Quickwit is offered under the AGPL v3.0 and as commercial software.
//  For commercial licensing, contact us at hello@quickwit.io.
//
//  AGPL:
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU Affero General Public License as
//  published by the Free Software Foundation, either version 3 of the
//  License, or (at your option) any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU Affero General Public License for more details.
//
//  You should have received a copy of the GNU Affero General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;

use tonic::{Request, Response, Status};

use quickwit_proto::cluster_service_server::ClusterService;
use quickwit_proto::{LeaveRequest, LeaveResult, Member as PMember, MembersRequest, MembersResult};

use crate::cluster::{Cluster, Member};

/// Convert the member state to the protobuf one.
impl From<Member> for PMember {
    fn from(member: Member) -> Self {
        PMember {
            id: member.host_key.to_string(),
            listen_address: member.listen_addr.to_string(),
            is_self: member.is_self,
        }
    }
}

/// Cluster service implementation.
/// This is a service to check the status of the cluster and to operate the cluster.
pub struct ClusterServiceImpl {
    /// Cluster.
    cluster: Arc<Cluster>,
}

impl ClusterServiceImpl {
    /// Create a cluster service given a cluster.
    pub fn new(cluster: Arc<Cluster>) -> Self {
        ClusterServiceImpl { cluster }
    }
}

#[tonic::async_trait]
impl ClusterService for ClusterServiceImpl {
    /// This is the API to get the list of cluster members.
    async fn members(
        &self,
        _request: Request<MembersRequest>,
    ) -> Result<Response<MembersResult>, Status> {
        let members = self
            .cluster
            .members()
            .into_iter()
            .map(PMember::from)
            .collect();

        Ok(Response::new(MembersResult { members }))
    }

    /// This is the API to leave the member from the cluster.
    async fn leave(
        &self,
        _request: Request<LeaveRequest>,
    ) -> Result<Response<LeaveResult>, Status> {
        self.cluster.leave();

        Ok(Response::new(LeaveResult {}))
    }
}

#[cfg(test)]
mod tests {
    use std::net::ToSocketAddrs;

    use uuid::Uuid;

    use quickwit_proto::Member as PMember;

    use crate::cluster::Member;

    #[tokio::test]
    async fn test_cluster_convert_proto_member() {
        let host_key = Uuid::new_v4();
        let listen_addr = "localhost:12345".to_socket_addrs().unwrap().next().unwrap();
        let is_self = true;

        let member = Member {
            host_key,
            listen_addr,
            is_self,
        };
        println!("member={:?}", member);

        let proto_member = PMember::from(member);
        println!("proto_member={:?}", proto_member);

        let expected = PMember {
            id: host_key.to_string(),
            listen_address: listen_addr.to_string(),
            is_self,
        };
        println!("expected={:?}", expected);

        assert_eq!(proto_member, expected);
    }
}
