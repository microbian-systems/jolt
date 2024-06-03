//! This is a port of https://github.com/microsoft/Nova/blob/main/src/provider/hyperkzg.rs
//!
//! This module implements `HyperKZG`, a KZG-based polynomial commitment for multilinear polynomials
//! HyperKZG is based on the transformation from univariate PCS to multilinear PCS in the Gemini paper (section 2.4.2 in <https://eprint.iacr.org/2022/420.pdf>).
//! However, there are some key differences:
//! (1) HyperKZG works with multilinear polynomials represented in evaluation form (rather than in coefficient form in Gemini's transformation).
//! This means that Spartan's polynomial IOP can use commit to its polynomials as-is without incurring any interpolations or FFTs.
//! (2) HyperKZG is specialized to use KZG as the univariate commitment scheme, so it includes several optimizations (both during the transformation of multilinear-to-univariate claims
//! and within the KZG commitment scheme implementation itself).
use super::kzg::{KZGProverKey, KZGVerifierKey, UnivariateKZG};
use crate::{
    msm::VariableBaseMSM,
    poly::{self, commitment::kzg::SRS, dense_mlpoly::DensePolynomial, unipoly::UniPoly},
    utils::{
        errors::ProofVerifyError,
        transcript::{AppendToTranscript, ProofTranscript},
    },
};
use ark_ec::{pairing::Pairing, AffineRepr};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_std::{One, Zero};
use rand_core::{CryptoRng, RngCore};
use rayon::iter::{
    IndexedParallelIterator, IntoParallelIterator, IntoParallelRefIterator,
    IntoParallelRefMutIterator, ParallelIterator,
};
use std::{marker::PhantomData, sync::Arc};

pub struct HyperKZGSRS<P: Pairing>(Arc<SRS<P>>);

impl<P: Pairing> HyperKZGSRS<P> {
    pub fn setup<R: RngCore + CryptoRng>(rng: &mut R, max_degree: usize) -> Self {
        Self(Arc::new(SRS::setup(rng, max_degree)))
    }

    pub fn trim(self, max_degree: usize) -> (HyperKZGProverKey<P>, HyperKZGVerifierKey<P>) {
        let (commit_pp, kzg_vk) = SRS::trim(self.0.clone(), max_degree);
        (
            HyperKZGProverKey { commit_pp },
            HyperKZGVerifierKey { kzg_vk },
        )
    }
}

#[derive(Clone, Debug)]
pub struct HyperKZGProverKey<P: Pairing> {
    pub commit_pp: KZGProverKey<P>,
}

#[derive(Copy, Clone, Debug)]
pub struct HyperKZGVerifierKey<P: Pairing> {
    pub kzg_vk: KZGVerifierKey<P>,
}

#[derive(Debug, PartialEq, CanonicalSerialize, CanonicalDeserialize)]
pub struct HyperKZGCommitment<P: Pairing>(P::G1Affine);

impl<P: Pairing> AppendToTranscript for HyperKZGCommitment<P> {
    fn append_to_transcript(&self, _label: &'static [u8], transcript: &mut ProofTranscript) {
        transcript.append_point(b"poly_commitment_share", &self.0.into_group());
    }
}

#[derive(Clone, CanonicalSerialize, CanonicalDeserialize, Debug)]
pub struct HyperKZGProof<P: Pairing> {
    com: Vec<P::G1Affine>,
    w: Vec<P::G1Affine>,
    v: Vec<Vec<P::ScalarField>>,
}

#[derive(Clone)]
pub struct HyperKZG<P: Pairing> {
    _phantom: PhantomData<P>,
}

impl<P: Pairing> HyperKZG<P>
where
    <P as Pairing>::ScalarField: poly::field::JoltField,
{
    pub fn protocol_name() -> &'static [u8] {
        b"HyperKZG"
    }

    pub fn commit(
        pp: &HyperKZGProverKey<P>,
        poly: &DensePolynomial<P::ScalarField>,
    ) -> Result<HyperKZGCommitment<P>, ProofVerifyError> {
        if pp.commit_pp.g1_powers().len() < poly.Z.len() {
            return Err(ProofVerifyError::KeyLengthError(
                pp.commit_pp.g1_powers().len(),
                poly.Z.len(),
            ));
        }
        Ok(HyperKZGCommitment(UnivariateKZG::commit(
            &pp.commit_pp,
            &UniPoly::from_coeff(poly.Z.clone()),
        )?))
    }

    pub fn open(
        pk: &HyperKZGProverKey<P>,
        transcript: &mut ProofTranscript,
        poly: &DensePolynomial<P::ScalarField>,
        point: &[P::ScalarField],
        _eval: &P::ScalarField,
    ) -> Result<HyperKZGProof<P>, ProofVerifyError> {
        let x: Vec<P::ScalarField> = point.to_vec();

        //////////////// begin helper closures //////////
        let kzg_open = |f: &[P::ScalarField], u: P::ScalarField| -> P::G1Affine {
            // On input f(x) and u compute the witness polynomial used to prove
            // that f(u) = v. The main part of this is to compute the
            // division (f(x) - f(u)) / (x - u), but we don't use a general
            // division algorithm, we make use of the fact that the division
            // never has a remainder, and that the denominator is always a linear
            // polynomial. The cost is (d-1) mults + (d-1) adds in P::ScalarField, where
            // d is the degree of f.
            //
            // We use the fact that if we compute the quotient of f(x)/(x-u),
            // there will be a remainder, but it'll be v = f(u).  Put another way
            // the quotient of f(x)/(x-u) and (f(x) - f(v))/(x-u) is the
            // same.  One advantage is that computing f(u) could be decoupled
            // from kzg_open, it could be done later or separate from computing W.

            let compute_witness_polynomial =
                |f: &[P::ScalarField], u: P::ScalarField| -> Vec<P::ScalarField> {
                    let d = f.len();

                    // Compute h(x) = f(x)/(x - u)
                    let mut h = vec![P::ScalarField::zero(); d];
                    for i in (1..d).rev() {
                        h[i - 1] = f[i] + h[i] * u;
                    }

                    h
                };

            let h = compute_witness_polynomial(f, u);

            UnivariateKZG::commit(&pk.commit_pp, &UniPoly::from_coeff(h)).unwrap()
        };

        let kzg_open_batch = |f: &[Vec<P::ScalarField>],
                              u: &[P::ScalarField],
                              transcript: &mut ProofTranscript|
         -> (Vec<P::G1Affine>, Vec<Vec<P::ScalarField>>) {
            let poly_eval = |f: &[P::ScalarField], u: P::ScalarField| -> P::ScalarField {
                let mut v = f[0];
                let mut u_power = P::ScalarField::one();

                for fi in f.iter().skip(1) {
                    u_power *= u;
                    v += u_power * fi;
                }

                v
            };

            let scalar_vector_muladd =
                |a: &mut Vec<P::ScalarField>, v: &Vec<P::ScalarField>, s: P::ScalarField| {
                    assert!(a.len() >= v.len());
                    for i in 0..v.len() {
                        a[i] += s * v[i];
                    }
                };

            let kzg_compute_batch_polynomial = |f: &[Vec<P::ScalarField>],
                                                q: P::ScalarField|
             -> Vec<P::ScalarField> {
                let k = f.len(); // Number of polynomials we're batching

                let batch_challenge_powers = |q: P::ScalarField, k: usize| -> Vec<P::ScalarField> {
                    // Compute powers of q : (1, q, q^2, ..., q^(k-1))
                    let mut q_powers = vec![P::ScalarField::one(); k];
                    for i in 1..k {
                        q_powers[i] = q_powers[i - 1] * q;
                    }
                    q_powers
                };

                let q_powers = batch_challenge_powers(q, k);

                // Compute B(x) = f[0] + q*f[1] + q^2 * f[2] + ... q^(k-1) * f[k-1]
                let mut B = f[0].clone();
                for i in 1..k {
                    scalar_vector_muladd(&mut B, &f[i], q_powers[i]); // B += q_powers[i] * f[i]
                }

                B
            };
            ///////// END kzg_open_batch closure helpers

            let k = f.len();
            let t = u.len();

            // The verifier needs f_i(u_j), so we compute them here
            // (V will compute B(u_j) itself)
            let mut v = vec![vec!(P::ScalarField::zero(); k); t];
            v.par_iter_mut().enumerate().for_each(|(i, v_i)| {
                // for each point u
                v_i.par_iter_mut().zip_eq(f).for_each(|(v_ij, f)| {
                    // for each poly f
                    // for each poly f (except the last one - since it is constant)
                    *v_ij = poly_eval(f, u[i]);
                });
            });

            transcript.append_scalars(
                b"v",
                &v.iter().flatten().cloned().collect::<Vec<P::ScalarField>>(),
            );
            let q = transcript.challenge_scalar(b"r");
            let B = kzg_compute_batch_polynomial(f, q);

            // Now open B at u0, ..., u_{t-1}
            let w = u
                .into_par_iter()
                .map(|ui| kzg_open(&B, *ui))
                .collect::<Vec<P::G1Affine>>();

            // The prover computes the challenge to keep the transcript in the same
            // state as that of the verifier
            transcript.append_points(
                b"W",
                &w.iter().map(|g| g.into_group()).collect::<Vec<P::G1>>(),
            );
            let _d_0: P::ScalarField = transcript.challenge_scalar(b"d");

            (w, v)
        };

        ///// END helper closures //////////

        let ell = x.len();
        let n = poly.len();
        assert_eq!(n, 1 << ell); // Below we assume that n is a power of two

        // Phase 1  -- create commitments com_1, ..., com_\ell
        // We do not compute final Pi (and its commitment) as it is constant and equals to 'eval'
        // also known to verifier, so can be derived on its side as well
        let mut polys: Vec<Vec<P::ScalarField>> = Vec::new();
        polys.push(poly.Z.to_vec());
        for i in 0..ell - 1 {
            let Pi_len = polys[i].len() / 2;
            let mut Pi = vec![P::ScalarField::zero(); Pi_len];

            #[allow(clippy::needless_range_loop)]
            Pi.par_iter_mut().enumerate().for_each(|(j, Pi_j)| {
                *Pi_j = x[ell - i - 1] * (polys[i][2 * j + 1] - polys[i][2 * j]) + polys[i][2 * j];
            });

            polys.push(Pi);
        }

        // We do not need to commit to the first polynomial as it is already committed.
        // Compute commitments in parallel
        let com: Vec<P::G1Affine> = (1..polys.len())
            .into_par_iter()
            .map(|i| {
                UnivariateKZG::commit(&pk.commit_pp, &UniPoly::from_coeff(polys[i].clone()))
                    .unwrap()
            })
            .collect();

        // Phase 2
        // We do not need to add x to the transcript, because in our context x was obtained from the transcript.
        // We also do not need to absorb `C` and `eval` as they are already absorbed by the transcript by the caller
        transcript.append_points(
            b"c",
            &com.iter().map(|g| g.into_group()).collect::<Vec<P::G1>>(),
        );
        let r: <P as Pairing>::ScalarField = transcript.challenge_scalar(b"c");
        let u = vec![r, -r, r * r];

        // Phase 3 -- create response
        let (w, v) = kzg_open_batch(&polys, &u, transcript);

        Ok(HyperKZGProof { com, w, v })
    }

    /// A method to verify purported evaluations of a batch of polynomials
    pub fn verify(
        vk: &HyperKZGVerifierKey<P>,
        transcript: &mut ProofTranscript,
        C: &HyperKZGCommitment<P>,
        point: &[P::ScalarField],
        P_of_x: &P::ScalarField,
        pi: &HyperKZGProof<P>,
    ) -> Result<(), ProofVerifyError> {
        let x = point.to_vec();
        let y = P_of_x;

        // vk is hashed in transcript already, so we do not add it here
        let kzg_verify_batch = |vk: &HyperKZGVerifierKey<P>,
                                C: &Vec<P::G1Affine>,
                                W: &Vec<P::G1Affine>,
                                u: &Vec<P::ScalarField>,
                                v: &Vec<Vec<P::ScalarField>>,
                                transcript: &mut ProofTranscript|
         -> bool {
            let k = C.len();
            let t = u.len();

            transcript.append_scalars(
                b"v",
                &v.iter().flatten().cloned().collect::<Vec<P::ScalarField>>(),
            );
            let q = transcript.challenge_scalar(b"r");
            let batch_challenge_powers = |q: P::ScalarField, k: usize| -> Vec<P::ScalarField> {
                // Compute powers of q : (1, q, q^2, ..., q^(k-1))
                let mut q_powers = vec![P::ScalarField::one(); k];
                for i in 1..k {
                    q_powers[i] = q_powers[i - 1] * q;
                }
                q_powers
            };

            let q_powers = batch_challenge_powers(q, k);

            transcript.append_points(
                b"W",
                &W.iter().map(|g| g.into_group()).collect::<Vec<P::G1>>(),
            );
            let d_0: P::ScalarField = transcript.challenge_scalar(b"d");
            let d_1 = d_0 * d_0;

            // Shorthand to convert from preprocessed G1 elements to non-preprocessed
            // let from_ppG1 = |P: &P::G1Affine| <E::GE as DlogGroup>::group(P);
            // // Shorthand to convert from preprocessed G2 elements to non-preprocessed
            // let from_ppG2 = |P: &P::G2Affine| <<E::GE as PairingGroup>::G2 as DlogGroup>::group(P);

            assert_eq!(t, 3);
            assert_eq!(W.len(), 3);
            // We write a special case for t=3, since this what is required for
            // hyperkzg. Following the paper directly, we must compute:
            // let L0 = C_B - vk.G * B_u[0] + W[0] * u[0];
            // let L1 = C_B - vk.G * B_u[1] + W[1] * u[1];
            // let L2 = C_B - vk.G * B_u[2] + W[2] * u[2];
            // let R0 = -W[0];
            // let R1 = -W[1];
            // let R2 = -W[2];
            // let L = L0 + L1*d_0 + L2*d_1;
            // let R = R0 + R1*d_0 + R2*d_1;
            //
            // We group terms to reduce the number of scalar mults (to seven):
            // In Rust, we could use MSMs for these, and speed up verification.
            //
            // Note, that while computing L, the intermediate computation of C_B together with computing
            // L0, L1, L2 can be replaced by single MSM of C with the powers of q multiplied by (1 + d_0 + d_1)
            // with additionally concatenated inputs for scalars/bases.

            let q_power_multiplier = P::ScalarField::one() + d_0 + d_1;

            let q_powers_multiplied: Vec<P::ScalarField> = q_powers
                .par_iter()
                .map(|q_power| *q_power * q_power_multiplier)
                .collect();

            // Compute the batched openings
            // compute B(u_i) = v[i][0] + q*v[i][1] + ... + q^(t-1) * v[i][t-1]
            let B_u = v
                .into_par_iter()
                .map(|v_i| {
                    v_i.into_par_iter()
                        .zip(q_powers.par_iter())
                        .map(|(a, b)| *a * b)
                        .sum()
                })
                .collect::<Vec<P::ScalarField>>();

            let L = <P::G1 as VariableBaseMSM>::msm(
                &[
                    &C[..k],
                    &[
                        W[0].clone(),
                        W[1].clone(),
                        W[2].clone(),
                        vk.kzg_vk.g1.clone(),
                    ],
                ]
                .concat(),
                &[
                    &q_powers_multiplied[..k],
                    &[
                        u[0],
                        (u[1] * d_0),
                        (u[2] * d_1),
                        -(B_u[0] + d_0 * B_u[1] + d_1 * B_u[2]),
                    ],
                ]
                .concat(),
            )
            .unwrap();

            let R = W[0] + W[1] * d_0 + W[2] * d_1;

            // Check that e(L, vk.H) == e(R, vk.tau_H)
            (P::pairing(&L, &vk.kzg_vk.g2)) == (P::pairing(&R, &vk.kzg_vk.beta_g2))
        };
        ////// END verify() closure helpers

        let ell = x.len();

        let mut com = pi.com.clone();

        // we do not need to add x to the transcript, because in our context x was
        // obtained from the transcript
        transcript.append_points(
            b"c",
            &com.iter().map(|g| g.into_group()).collect::<Vec<P::G1>>(),
        );
        let r: <P as Pairing>::ScalarField = transcript.challenge_scalar(b"c");

        if r == P::ScalarField::zero() || C.0 == P::G1Affine::zero() {
            return Err(ProofVerifyError::InternalError);
        }
        com.insert(0, C.0); // set com_0 = C, shifts other commitments to the right

        let u = vec![r, -r, r * r];

        // Setup vectors (Y, ypos, yneg) from pi.v
        let v = &pi.v;
        if v.len() != 3 {
            return Err(ProofVerifyError::InternalError);
        }
        if v[0].len() != ell || v[1].len() != ell || v[2].len() != ell {
            return Err(ProofVerifyError::InternalError);
        }
        let ypos = &v[0];
        let yneg = &v[1];
        let mut Y = v[2].to_vec();
        Y.push(*y);

        // Check consistency of (Y, ypos, yneg)
        let two = P::ScalarField::from(2u64);
        for i in 0..ell {
            if two * r * Y[i + 1]
                != r * (P::ScalarField::one() - x[ell - i - 1]) * (ypos[i] + yneg[i])
                    + x[ell - i - 1] * (ypos[i] - yneg[i])
            {
                return Err(ProofVerifyError::InternalError);
            }
            // Note that we don't make any checks about Y[0] here, but our batching
            // check below requires it
        }

        // Check commitments to (Y, ypos, yneg) are valid
        if !kzg_verify_batch(vk, &com, &pi.w, &u, &pi.v, transcript) {
            return Err(ProofVerifyError::InternalError);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ark_bn254::{Bn254, Fr};
    use ark_std::UniformRand;
    use rand_core::SeedableRng;

    #[test]
    fn test_hyperkzg_eval() {
        // Test with poly(X1, X2) = 1 + X1 + X2 + X1*X2
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(0);
        let srs = HyperKZGSRS::setup(&mut rng, 3);
        let (pk, vk): (HyperKZGProverKey<Bn254>, HyperKZGVerifierKey<Bn254>) = srs.trim(3);

        // poly is in eval. representation; evaluated at [(0,0), (0,1), (1,0), (1,1)]
        let poly = DensePolynomial::new(vec![Fr::from(1), Fr::from(2), Fr::from(2), Fr::from(4)]);

        let C = HyperKZG::commit(&pk, &poly).unwrap();

        let test_inner = |point: Vec<Fr>, eval: Fr| -> Result<(), ProofVerifyError> {
            let mut tr = ProofTranscript::new(b"TestEval");
            let proof = HyperKZG::open(&pk, &mut tr, &poly, &point, &eval).unwrap();
            let mut tr = ProofTranscript::new(b"TestEval");
            HyperKZG::verify(&vk, &mut tr, &C, &point, &eval, &proof)
        };

        // Call the prover with a (point, eval) pair.
        // The prover does not recompute so it may produce a proof, but it should not verify
        let point = vec![Fr::from(0), Fr::from(0)];
        let eval = Fr::from(1);
        assert!(test_inner(point, eval).is_ok());

        let point = vec![Fr::from(0), Fr::from(1)];
        let eval = Fr::from(2);
        assert!(test_inner(point, eval).is_ok());

        let point = vec![Fr::from(1), Fr::from(1)];
        let eval = Fr::from(4);
        assert!(test_inner(point, eval).is_ok());

        let point = vec![Fr::from(0), Fr::from(2)];
        let eval = Fr::from(3);
        assert!(test_inner(point, eval).is_ok());

        let point = vec![Fr::from(2), Fr::from(2)];
        let eval = Fr::from(9);
        assert!(test_inner(point, eval).is_ok());

        // Try a couple incorrect evaluations and expect failure
        let point = vec![Fr::from(2), Fr::from(2)];
        let eval = Fr::from(50);
        assert!(test_inner(point, eval).is_err());

        let point = vec![Fr::from(0), Fr::from(2)];
        let eval = Fr::from(4);
        assert!(test_inner(point, eval).is_err());
    }

    #[test]
    fn test_hyperkzg_small() {
        let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(0);

        // poly = [1, 2, 1, 4]
        let poly = DensePolynomial::new(vec![Fr::from(1), Fr::from(2), Fr::from(1), Fr::from(4)]);

        // point = [4,3]
        let point = vec![Fr::from(4), Fr::from(3)];

        // eval = 28
        let eval = Fr::from(28);

        let srs = HyperKZGSRS::setup(&mut rng, 3);
        let (pk, vk): (HyperKZGProverKey<Bn254>, HyperKZGVerifierKey<Bn254>) = srs.trim(3);

        // make a commitment
        let C = HyperKZG::commit(&pk, &poly).unwrap();

        // prove an evaluation
        let mut tr = ProofTranscript::new(b"TestEval");
        let proof = HyperKZG::open(&pk, &mut tr, &poly, &point, &eval).unwrap();
        let post_c_p = tr.challenge_scalar::<Fr>(b"c");

        // verify the evaluation
        let mut verifier_transcript = ProofTranscript::new(b"TestEval");
        assert!(HyperKZG::verify(&vk, &mut verifier_transcript, &C, &point, &eval, &proof).is_ok());
        let post_c_v = verifier_transcript.challenge_scalar::<Fr>(b"c");

        // check if the prover transcript and verifier transcript are kept in the same state
        assert_eq!(post_c_p, post_c_v);

        let mut proof_bytes = Vec::new();
        proof.serialize_compressed(&mut proof_bytes).unwrap();
        assert_eq!(proof_bytes.len(), 368);

        // Change the proof and expect verification to fail
        let mut bad_proof = proof.clone();
        let v1 = bad_proof.v[1].clone();
        bad_proof.v[0].clone_from(&v1);
        let mut verifier_transcript2 = ProofTranscript::new(b"TestEval");
        assert!(HyperKZG::verify(
            &vk,
            &mut verifier_transcript2,
            &C,
            &point,
            &eval,
            &bad_proof
        )
        .is_err());
    }

    #[test]
    fn test_hyperkzg_large() {
        // test the hyperkzg prover and verifier with random instances (derived from a seed)
        for ell in [4, 5, 6] {
            let mut rng = rand_chacha::ChaCha20Rng::seed_from_u64(ell as u64);

            let n = 1 << ell; // n = 2^ell

            let poly = DensePolynomial::new(
                (0..n)
                    .map(|_| <Bn254 as Pairing>::ScalarField::rand(&mut rng))
                    .collect::<Vec<_>>(),
            );
            let point = (0..ell)
                .map(|_| <Bn254 as Pairing>::ScalarField::rand(&mut rng))
                .collect::<Vec<_>>();
            let eval = poly.evaluate(&point);

            let srs = HyperKZGSRS::setup(&mut rng, n);
            let (pk, vk): (HyperKZGProverKey<Bn254>, HyperKZGVerifierKey<Bn254>) = srs.trim(n);

            // make a commitment
            let C = HyperKZG::commit(&pk, &poly).unwrap();

            // prove an evaluation
            let mut prover_transcript = ProofTranscript::new(b"TestEval");
            let proof: HyperKZGProof<Bn254> =
                HyperKZG::open(&pk, &mut prover_transcript, &poly, &point, &eval).unwrap();

            // verify the evaluation
            let mut verifier_tr = ProofTranscript::new(b"TestEval");
            assert!(HyperKZG::verify(&vk, &mut verifier_tr, &C, &point, &eval, &proof).is_ok());

            // Change the proof and expect verification to fail
            let mut bad_proof = proof.clone();
            let v1 = bad_proof.v[1].clone();
            bad_proof.v[0].clone_from(&v1);
            let mut verifier_tr2 = ProofTranscript::new(b"TestEval");
            assert!(
                HyperKZG::verify(&vk, &mut verifier_tr2, &C, &point, &eval, &bad_proof).is_err()
            );
        }
    }
}
