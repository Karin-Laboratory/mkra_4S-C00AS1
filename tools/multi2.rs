// SPDX-License-Identifier: Apache-2.0
// Scalar MULTI2 for saved-TS diagnostics and the live server. Algorithm and CBC/OFB
// behavior checked against stz2012/libarib25 93ff3afd0a13f42724b20564873066f470825e78.
// See third-party/libarib25-NOTICE.md. No key data is logged or persisted.
type Block = [u32;2];
pub type WorkKey = [u32;8];
fn load(p:&[u8])->Block {[u32::from_be_bytes(p[..4].try_into().unwrap()),u32::from_be_bytes(p[4..8].try_into().unwrap())]}
fn bytes(p:Block)->[u8;8] {let mut b=[0;8];b[..4].copy_from_slice(&p[0].to_be_bytes());b[4..].copy_from_slice(&p[1].to_be_bytes());b}
fn one(p:&mut Block){p[1]^=p[0]}
fn two(p:&mut Block,k:u32){let y=p[1].wrapping_add(k);let z=y.rotate_left(1).wrapping_add(y).wrapping_sub(1);p[0]^=z.rotate_left(4)^z;}
fn three(p:&mut Block,k1:u32,k2:u32){let y=p[0].wrapping_add(k1);let z=y.rotate_left(2).wrapping_add(y).wrapping_add(1);let b=(z.rotate_left(8)^z).wrapping_add(k2);let c=b.wrapping_add(b>>31);p[1]^=c.rotate_left(16)^(c|p[0]);}
fn four(p:&mut Block,k:u32){let y=p[1].wrapping_add(k);p[0]^=y.rotate_left(2).wrapping_add(y).wrapping_add(1);}
pub fn schedule(system:&[u8;32],control:&[u8;8])->WorkKey {
    let s:Vec<u32>=system.chunks_exact(4).map(|b|u32::from_be_bytes(b.try_into().unwrap())).collect();
    let mut p=load(control);let mut w=[0;8];
    one(&mut p);two(&mut p,s[0]);w[0]=p[0];three(&mut p,s[1],s[2]);w[1]=p[1];four(&mut p,s[3]);w[2]=p[0];one(&mut p);w[3]=p[1];
    two(&mut p,s[4]);w[4]=p[0];three(&mut p,s[5],s[6]);w[5]=p[1];four(&mut p,s[7]);w[6]=p[0];one(&mut p);w[7]=p[1];w
}
fn encrypt_block(mut p:Block,w:&WorkKey)->Block {
    for _ in 0..4 {one(&mut p);two(&mut p,w[0]);three(&mut p,w[1],w[2]);four(&mut p,w[3]);one(&mut p);two(&mut p,w[4]);three(&mut p,w[5],w[6]);four(&mut p,w[7]);}p
}
fn decrypt_block(mut p:Block,w:&WorkKey)->Block {
    for _ in 0..4 {four(&mut p,w[7]);three(&mut p,w[5],w[6]);two(&mut p,w[4]);one(&mut p);four(&mut p,w[3]);three(&mut p,w[1],w[2]);two(&mut p,w[0]);one(&mut p);}p
}
pub fn decrypt(payload:&mut [u8],iv:&[u8;8],work:&WorkKey) {
    let mut state=load(iv);let n=payload.len()/8*8;
    for block in payload[..n].chunks_exact_mut(8) {
        let encrypted=load(block);let plain=decrypt_block(encrypted,work);
        block.copy_from_slice(&bytes([plain[0]^state[0],plain[1]^state[1]]));state=encrypted;
    }
    let mask=bytes(encrypt_block(state,work));
    for (b,k) in payload[n..].iter_mut().zip(mask){*b^=k;}
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn cbc_and_residual_ofb_match_independent_upstream_vectors(){
        let system: [u8;32]=std::array::from_fn(|i|i as u8);
        let control:[u8;8]=std::array::from_fn(|i|0x80+i as u8);
        let iv:[u8;8]=std::array::from_fn(|i|0xf0+i as u8);
        let work=schedule(&system,&control);
        for line in include_str!("fixtures/multi2-reference.txt").lines(){
            let (n,hex)=line.split_once(' ').unwrap();let n:usize=n.parse().unwrap();
            let mut data:Vec<u8>=(0..hex.len()).step_by(2).map(|i|u8::from_str_radix(&hex[i..i+2],16).unwrap()).collect();
            assert_eq!(data.len(),n);decrypt(&mut data,&iv,&work);
            assert_eq!(data,(0..n).map(|i|((i*37+11)&255)as u8).collect::<Vec<_>>(),"length {n}");
        }
    }
}
