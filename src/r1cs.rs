//! This module defines R1CS related types and a folding scheme for Relaxed R1CS
#![allow(clippy::type_complexity)]
use crate::{
  errors::SpartanError,
  traits::{commitment::CommitmentEngineTrait, Group, TranscriptReprTrait},
  Commitment, CommitmentKey, CE,
};
use core::{cmp::max, marker::PhantomData};
use ff::Field;
use itertools::concat;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Public parameters for a given R1CS
#[derive(Clone, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct R1CS<G: Group> {
  _p: PhantomData<G>,
}

/// A type that holds the shape of the R1CS matrices
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct R1CSShape<G: Group> {
  pub(crate) num_cons: usize,
  pub(crate) num_vars: usize,
  pub(crate) num_io: usize,
  pub(crate) A: Vec<(usize, usize, G::Scalar)>,
  pub(crate) B: Vec<(usize, usize, G::Scalar)>,
  pub(crate) C: Vec<(usize, usize, G::Scalar)>,
}

/// A type that holds a witness for a given R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct R1CSWitness<G: Group> {
  pub(crate) W: Vec<G::Scalar>,
}

/// A type that holds an R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct R1CSInstance<G: Group> {
  pub(crate) comm_W: Commitment<G>,
  pub(crate) X: Vec<G::Scalar>,
}

/// A type that holds a witness for a given Relaxed R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelaxedR1CSWitness<G: Group> {
  pub(crate) W: Vec<G::Scalar>,
  pub(crate) E: Vec<G::Scalar>,
}

/// A type that holds a Relaxed R1CS instance
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct RelaxedR1CSInstance<G: Group> {
  pub(crate) comm_W: Commitment<G>,
  pub(crate) comm_E: Commitment<G>,
  pub(crate) X: Vec<G::Scalar>,
  pub(crate) u: G::Scalar,
}

impl<G: Group> R1CS<G> {
  /// Samples public parameters for the specified number of constraints and variables in an R1CS
  pub fn commitment_key(S: &R1CSShape<G>) -> CommitmentKey<G> {
    let S = S.pad(); // pad the shape before computing the commitment key
    let num_cons = S.num_cons;
    let num_vars = S.num_vars;
    G::CE::setup(b"ck", max(num_cons, num_vars))
  }
}

impl<G: Group> R1CSShape<G> {
  /// Create an object of type `R1CSShape` from the explicitly specified R1CS matrices
  #[tracing::instrument(skip_all, name = "R1CSShape::new")]
  pub fn new(
    num_cons: usize,
    num_vars: usize,
    num_io: usize,
    A: &[(usize, usize, G::Scalar)],
    B: &[(usize, usize, G::Scalar)],
    C: &[(usize, usize, G::Scalar)],
  ) -> Result<R1CSShape<G>, SpartanError> {
    let is_valid = |num_cons: usize,
                    num_vars: usize,
                    num_io: usize,
                    M: &[(usize, usize, G::Scalar)]|
     -> Result<(), SpartanError> {
      let res = (0..M.len())
        .map(|i| {
          let (row, col, _val) = M[i];
          if row >= num_cons || col > num_io + num_vars {
            Err(SpartanError::InvalidIndex)
          } else {
            Ok(())
          }
        })
        .collect::<Result<Vec<()>, SpartanError>>();

      if res.is_err() {
        Err(SpartanError::InvalidIndex)
      } else {
        Ok(())
      }
    };

    let res_A = is_valid(num_cons, num_vars, num_io, A);
    let res_B = is_valid(num_cons, num_vars, num_io, B);
    let res_C = is_valid(num_cons, num_vars, num_io, C);

    if res_A.is_err() || res_B.is_err() || res_C.is_err() {
      return Err(SpartanError::InvalidIndex);
    }

    let shape = R1CSShape {
      num_cons,
      num_vars,
      num_io,
      A: A.to_owned(),
      B: B.to_owned(),
      C: C.to_owned(),
    };

    // pad the shape
    Ok(shape.pad())
  }

  // Checks regularity conditions on the R1CSShape, required in Spartan-class SNARKs
  // Panics if num_cons, num_vars, or num_io are not powers of two, or if num_io > num_vars
  #[inline]
  pub(crate) fn check_regular_shape(&self) {
    assert_eq!(self.num_cons.next_power_of_two(), self.num_cons);
    assert_eq!(self.num_vars.next_power_of_two(), self.num_vars);
    assert!(self.num_io < self.num_vars);
  }

  #[tracing::instrument(skip_all, name = "R1CSShape::multiply_vec")]
  pub fn multiply_vec(
    &self,
    z: &[G::Scalar],
  ) -> Result<(Vec<G::Scalar>, Vec<G::Scalar>, Vec<G::Scalar>), SpartanError> {
    if z.len() != self.num_io + self.num_vars + 1 {
      return Err(SpartanError::InvalidWitnessLength);
    }

    // computes a product between a sparse matrix `M` and a vector `z`
    // This does not perform any validation of entries in M (e.g., if entries in `M` reference indexes outside the range of `z`)
    // This is safe since we know that `M` is valid
    let sparse_matrix_vec_product =
      |M: &Vec<(usize, usize, G::Scalar)>, num_rows: usize, z: &[G::Scalar]| -> Vec<G::Scalar> {

        // Parallelism strategy below splits the (row, column, value) tuples into num_threads different chunks.
        // It is assumed that the tuples are (row, column) ordered. We exploit this fact to create a mutex over
        // each of the chunks and assume that only one of the threads will be writing to each chunk at a time
        // due to ordering.

        let num_threads = rayon::current_num_threads() * 4; // Enable work stealing incase of thread work imbalance
        let thread_chunk_size = M.len() / num_threads;
        let row_chunk_size = (num_rows as f64 / num_threads as f64).ceil() as usize;

        let mut chunks: Vec<std::sync::Mutex<Vec<G::Scalar>>> = Vec::with_capacity(num_threads);
        let mut remaining_rows = num_rows;
        (0..num_threads).for_each(|i| {
          if i == num_threads - 1 { // the final chunk may be smaller
            let inner = std::sync::Mutex::new(vec![G::Scalar::ZERO; remaining_rows]);
            chunks.push(inner);
          } else {
            let inner = std::sync::Mutex::new(vec![G::Scalar::ZERO; row_chunk_size]);
            chunks.push(inner);
            remaining_rows -= row_chunk_size;
          }
        });

        let get_chunk = |row_index: usize| -> usize { row_index / row_chunk_size };
        let get_index = |row_index: usize| -> usize { row_index % row_chunk_size };

        let span = tracing::span!(tracing::Level::TRACE, "all_chunks_multiplication");
        let _enter = span.enter();
        M.par_chunks(thread_chunk_size).for_each(|sub_matrix: &[(usize, usize, G::Scalar)]| {
          let (init_row, init_col, init_val) = sub_matrix[0];
          let mut prev_chunk_index = get_chunk(init_row);
          let curr_row_index = get_index(init_row);
          let mut curr_chunk = chunks[prev_chunk_index].lock().unwrap();

          curr_chunk[curr_row_index] += init_val * z[init_col];

          let span_a = tracing::span!(tracing::Level::TRACE, "chunk_multiplication");
          let _enter_b = span_a.enter();
          for (row, col, val) in sub_matrix.iter().skip(1) {
            let curr_chunk_index = get_chunk(*row);
            if prev_chunk_index != curr_chunk_index { // only unlock the mutex again if required
              drop(curr_chunk); // drop the curr_chunk before waiting for the next to avoid race condition
              let new_chunk = chunks[curr_chunk_index].lock().unwrap();
              curr_chunk = new_chunk;

              prev_chunk_index = curr_chunk_index;
            }

            if z[*col].is_zero_vartime() { 
              continue; 
            }

            let m = if z[*col].eq(&G::Scalar::ONE) {
              *val
            } else if val.eq(&G::Scalar::ONE) {
              z[*col]
            } else {
              *val * z[*col]
            };
            curr_chunk[get_index(*row)] += m;
          }
        });
        drop(_enter);
        drop(span);

        let span_a = tracing::span!(tracing::Level::TRACE, "chunks_mutex_unwrap");
        let _enter_a = span_a.enter();
        // TODO(sragss): Mutex unwrap takes about 30% of the time due to clone, likely unnecessary.
        let mut flat_chunks: Vec<G::Scalar> = Vec::with_capacity(num_rows);
        for chunk in chunks {
          let inner_vec = chunk.into_inner().unwrap();
          flat_chunks.extend(inner_vec.iter());
        }
        drop(_enter_a);
        drop(span_a);


        flat_chunks
      };

    let (Az, (Bz, Cz)) = rayon::join(
      || sparse_matrix_vec_product(&self.A, self.num_cons, z),
      || {
        rayon::join(
          || sparse_matrix_vec_product(&self.B, self.num_cons, z),
          || sparse_matrix_vec_product(&self.C, self.num_cons, z),
        )
      },
    );

    Ok((Az, Bz, Cz))
  }

  /// Checks if the Relaxed R1CS instance is satisfiable given a witness and its shape
  pub fn is_sat_relaxed(
    &self,
    ck: &CommitmentKey<G>,
    U: &RelaxedR1CSInstance<G>,
    W: &RelaxedR1CSWitness<G>,
  ) -> Result<(), SpartanError> {
    assert_eq!(W.W.len(), self.num_vars);
    assert_eq!(W.E.len(), self.num_cons);
    assert_eq!(U.X.len(), self.num_io);

    // verify if Az * Bz = u*Cz + E
    let res_eq: bool = {
      let z = concat(vec![W.W.clone(), vec![U.u], U.X.clone()]);
      let (Az, Bz, Cz) = self.multiply_vec(&z)?;
      assert_eq!(Az.len(), self.num_cons);
      assert_eq!(Bz.len(), self.num_cons);
      assert_eq!(Cz.len(), self.num_cons);

      let res: usize = (0..self.num_cons)
        .map(|i| usize::from(Az[i] * Bz[i] != U.u * Cz[i] + W.E[i]))
        .sum();

      res == 0
    };

    // verify if comm_E and comm_W are commitments to E and W
    let res_comm: bool = {
      let (comm_W, comm_E) =
        rayon::join(|| CE::<G>::commit(ck, &W.W), || CE::<G>::commit(ck, &W.E));
      U.comm_W == comm_W && U.comm_E == comm_E
    };

    if res_eq && res_comm {
      Ok(())
    } else {
      Err(SpartanError::UnSat)
    }
  }

  /// Checks if the R1CS instance is satisfiable given a witness and its shape
  pub fn is_sat(
    &self,
    ck: &CommitmentKey<G>,
    U: &R1CSInstance<G>,
    W: &R1CSWitness<G>,
  ) -> Result<(), SpartanError> {
    assert_eq!(W.W.len(), self.num_vars);
    assert_eq!(U.X.len(), self.num_io);

    // verify if Az * Bz = u*Cz
    let res_eq: bool = {
      let z = concat(vec![W.W.clone(), vec![G::Scalar::ONE], U.X.clone()]);
      let (Az, Bz, Cz) = self.multiply_vec(&z)?;
      assert_eq!(Az.len(), self.num_cons);
      assert_eq!(Bz.len(), self.num_cons);
      assert_eq!(Cz.len(), self.num_cons);

      let res: usize = (0..self.num_cons)
        .map(|i| usize::from(Az[i] * Bz[i] != Cz[i]))
        .sum();

      res == 0
    };

    // verify if comm_W is a commitment to W
    let res_comm: bool = U.comm_W == CE::<G>::commit(ck, &W.W);

    if res_eq && res_comm {
      Ok(())
    } else {
      Err(SpartanError::UnSat)
    }
  }

  /// A method to compute a commitment to the cross-term `T` given a
  /// Relaxed R1CS instance-witness pair and an R1CS instance-witness pair
  pub fn commit_T(
    &self,
    ck: &CommitmentKey<G>,
    U1: &RelaxedR1CSInstance<G>,
    W1: &RelaxedR1CSWitness<G>,
    U2: &R1CSInstance<G>,
    W2: &R1CSWitness<G>,
  ) -> Result<(Vec<G::Scalar>, Commitment<G>), SpartanError> {
    let (AZ_1, BZ_1, CZ_1) = {
      let Z1 = concat(vec![W1.W.clone(), vec![U1.u], U1.X.clone()]);
      self.multiply_vec(&Z1)?
    };

    let (AZ_2, BZ_2, CZ_2) = {
      let Z2 = concat(vec![W2.W.clone(), vec![G::Scalar::ONE], U2.X.clone()]);
      self.multiply_vec(&Z2)?
    };

    let AZ_1_circ_BZ_2 = (0..AZ_1.len())
      .into_par_iter()
      .map(|i| AZ_1[i] * BZ_2[i])
      .collect::<Vec<G::Scalar>>();
    let AZ_2_circ_BZ_1 = (0..AZ_2.len())
      .into_par_iter()
      .map(|i| AZ_2[i] * BZ_1[i])
      .collect::<Vec<G::Scalar>>();
    let u_1_cdot_CZ_2 = (0..CZ_2.len())
      .into_par_iter()
      .map(|i| U1.u * CZ_2[i])
      .collect::<Vec<G::Scalar>>();
    let u_2_cdot_CZ_1 = (0..CZ_1.len())
      .into_par_iter()
      .map(|i| CZ_1[i])
      .collect::<Vec<G::Scalar>>();

    let T = AZ_1_circ_BZ_2
      .par_iter()
      .zip(&AZ_2_circ_BZ_1)
      .zip(&u_1_cdot_CZ_2)
      .zip(&u_2_cdot_CZ_1)
      .map(|(((a, b), c), d)| *a + *b - *c - *d)
      .collect::<Vec<G::Scalar>>();

    let comm_T = CE::<G>::commit(ck, &T);

    Ok((T, comm_T))
  }

  /// Pads the R1CSShape so that the number of variables is a power of two
  /// Renumbers variables to accomodate padded variables
  pub fn pad(&self) -> Self {
    // equalize the number of variables and constraints
    let m = max(self.num_vars, self.num_cons).next_power_of_two();

    // check if the provided R1CSShape is already as required
    if self.num_vars == m && self.num_cons == m {
      return self.clone();
    }

    // check if the number of variables are as expected, then
    // we simply set the number of constraints to the next power of two
    if self.num_vars == m {
      return R1CSShape {
        num_cons: m,
        num_vars: m,
        num_io: self.num_io,
        A: self.A.clone(),
        B: self.B.clone(),
        C: self.C.clone(),
      };
    }

    // otherwise, we need to pad the number of variables and renumber variable accesses
    let num_vars_padded = m;
    let num_cons_padded = m;
    let apply_pad = |M: &[(usize, usize, G::Scalar)]| -> Vec<(usize, usize, G::Scalar)> {
      M.par_iter()
        .map(|(r, c, v)| {
          (
            *r,
            if c >= &self.num_vars {
              c + num_vars_padded - self.num_vars
            } else {
              *c
            },
            *v,
          )
        })
        .collect::<Vec<_>>()
    };

    let A_padded = apply_pad(&self.A);
    let B_padded = apply_pad(&self.B);
    let C_padded = apply_pad(&self.C);

    R1CSShape {
      num_cons: num_cons_padded,
      num_vars: num_vars_padded,
      num_io: self.num_io,
      A: A_padded,
      B: B_padded,
      C: C_padded,
    }
  }
}

impl<G: Group> R1CSWitness<G> {
  /// A method to create a witness object using a vector of scalars
  pub fn new(S: &R1CSShape<G>, W: &[G::Scalar]) -> Result<R1CSWitness<G>, SpartanError> {
    let w = R1CSWitness { W: W.to_owned() };
    Ok(w.pad(S))
  }

  /// Pads the provided witness to the correct length
  pub fn pad(&self, S: &R1CSShape<G>) -> R1CSWitness<G> {
    let W = {
      let mut W = self.W.clone();
      W.extend(vec![G::Scalar::ZERO; S.num_vars - W.len()]);
      W
    };

    Self { W }
  }

  /// Commits to the witness using the supplied generators
  #[tracing::instrument(skip_all, name = "R1CSWitness::commit")]
  pub fn commit(&self, ck: &CommitmentKey<G>) -> Commitment<G> {
    CE::<G>::commit(ck, &self.W)
  }
}

impl<G: Group> R1CSInstance<G> {
  /// A method to create an instance object using consitituent elements
  pub fn new(
    S: &R1CSShape<G>,
    comm_W: &Commitment<G>,
    X: &[G::Scalar],
  ) -> Result<R1CSInstance<G>, SpartanError> {
    if S.num_io != X.len() {
      Err(SpartanError::InvalidInputLength)
    } else {
      Ok(R1CSInstance {
        comm_W: comm_W.clone(),
        X: X.to_owned(),
      })
    }
  }
}

impl<G: Group> TranscriptReprTrait<G> for R1CSInstance<G> {
  fn to_transcript_bytes(&self) -> Vec<u8> {
    [
      self.comm_W.to_transcript_bytes(),
      self.X.as_slice().to_transcript_bytes(),
    ]
    .concat()
  }
}

impl<G: Group> RelaxedR1CSWitness<G> {
  /// Produces a default RelaxedR1CSWitness given an R1CSShape
  pub fn default(S: &R1CSShape<G>) -> RelaxedR1CSWitness<G> {
    RelaxedR1CSWitness {
      W: vec![G::Scalar::ZERO; S.num_vars],
      E: vec![G::Scalar::ZERO; S.num_cons],
    }
  }

  /// Initializes a new RelaxedR1CSWitness from an R1CSWitness
  #[tracing::instrument(skip_all, name = "RelaxedR1CSWitness::from_r1cs_witness")]
  pub fn from_r1cs_witness(S: &R1CSShape<G>, witness: &R1CSWitness<G>) -> RelaxedR1CSWitness<G> {
    RelaxedR1CSWitness {
      W: witness.W.clone(),
      E: vec![G::Scalar::ZERO; S.num_cons],
    }
  }

  /// Commits to the witness using the supplied generators
  pub fn commit(&self, ck: &CommitmentKey<G>) -> (Commitment<G>, Commitment<G>) {
    (CE::<G>::commit(ck, &self.W), CE::<G>::commit(ck, &self.E))
  }

  /// Folds an incoming R1CSWitness into the current one
  pub fn fold(
    &self,
    W2: &R1CSWitness<G>,
    T: &[G::Scalar],
    r: &G::Scalar,
  ) -> Result<RelaxedR1CSWitness<G>, SpartanError> {
    let (W1, E1) = (&self.W, &self.E);
    let W2 = &W2.W;

    if W1.len() != W2.len() {
      return Err(SpartanError::InvalidWitnessLength);
    }

    let W = W1
      .par_iter()
      .zip(W2)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<G::Scalar>>();
    let E = E1
      .par_iter()
      .zip(T)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<G::Scalar>>();
    Ok(RelaxedR1CSWitness { W, E })
  }

  /// Pads the provided witness to the correct length
  pub fn pad(&self, S: &R1CSShape<G>) -> RelaxedR1CSWitness<G> {
    let W = {
      let mut W = self.W.clone();
      W.extend(vec![G::Scalar::ZERO; S.num_vars - W.len()]);
      W
    };

    let E = {
      let mut E = self.E.clone();
      E.extend(vec![G::Scalar::ZERO; S.num_cons - E.len()]);
      E
    };

    Self { W, E }
  }
}

impl<G: Group> RelaxedR1CSInstance<G> {
  /// Produces a default RelaxedR1CSInstance given R1CSGens and R1CSShape
  pub fn default(_ck: &CommitmentKey<G>, S: &R1CSShape<G>) -> RelaxedR1CSInstance<G> {
    let (comm_W, comm_E) = (Commitment::<G>::default(), Commitment::<G>::default());
    RelaxedR1CSInstance {
      comm_W,
      comm_E,
      u: G::Scalar::ZERO,
      X: vec![G::Scalar::ZERO; S.num_io],
    }
  }

  /// Initializes a new RelaxedR1CSInstance from an R1CSInstance
  pub fn from_r1cs_instance(
    ck: &CommitmentKey<G>,
    S: &R1CSShape<G>,
    instance: &R1CSInstance<G>,
  ) -> RelaxedR1CSInstance<G> {
    let mut r_instance = RelaxedR1CSInstance::default(ck, S);
    r_instance.comm_W = instance.comm_W.clone();
    r_instance.u = G::Scalar::ONE;
    r_instance.X = instance.X.clone();
    r_instance
  }

  /// Initializes a new RelaxedR1CSInstance from an R1CSInstance
  #[tracing::instrument(skip_all, name = "RelaxedR1CSInstance::from_r1cs_instance_unchecked")]
  pub fn from_r1cs_instance_unchecked(
    comm_W: &Commitment<G>,
    X: &[G::Scalar],
  ) -> RelaxedR1CSInstance<G> {
    RelaxedR1CSInstance {
      comm_W: comm_W.clone(),
      comm_E: Commitment::<G>::default(),
      u: G::Scalar::ONE,
      X: X.to_vec(),
    }
  }

  /// Folds an incoming RelaxedR1CSInstance into the current one
  pub fn fold(
    &self,
    U2: &R1CSInstance<G>,
    comm_T: &Commitment<G>,
    r: &G::Scalar,
  ) -> Result<RelaxedR1CSInstance<G>, SpartanError> {
    let (X1, u1, comm_W_1, comm_E_1) =
      (&self.X, &self.u, &self.comm_W.clone(), &self.comm_E.clone());
    let (X2, comm_W_2) = (&U2.X, &U2.comm_W);

    // weighted sum of X, comm_W, comm_E, and u
    let X = X1
      .par_iter()
      .zip(X2)
      .map(|(a, b)| *a + *r * *b)
      .collect::<Vec<G::Scalar>>();
    let comm_W = comm_W_1.clone() + comm_W_2.clone() * *r;
    let comm_E = comm_E_1.clone() + comm_T.clone() * *r;
    let u = *u1 + *r;

    Ok(RelaxedR1CSInstance {
      comm_W,
      comm_E,
      X,
      u,
    })
  }
}

impl<G: Group> TranscriptReprTrait<G> for RelaxedR1CSInstance<G> {
  fn to_transcript_bytes(&self) -> Vec<u8> {
    [
      self.comm_W.to_transcript_bytes(),
      self.comm_E.to_transcript_bytes(),
      self.u.to_transcript_bytes(),
      self.X.as_slice().to_transcript_bytes(),
    ]
    .concat()
  }
}
