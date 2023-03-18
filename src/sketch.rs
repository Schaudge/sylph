use crate::cmdline::*;
use needletail::parse_fastx_file;
use rayon::prelude::*;
use std::path::Path;
use std::fs::File;
use std::io::{BufWriter};
use log::*;
use std::io::{BufReader};
use crate::seeding::*;
use crate::types::*;
use flate2::read::GzDecoder;
use seq_io::fasta::{Reader as ReaderA, Record as ReccordA};
use seq_io::fastq::{Reader as ReaderQ, Record as RecordQ};
use seq_io::parallel::read_parallel;
use std::collections::HashMap;
use std::io::Read;
use regex::Regex;

fn combine_sketches(mut sketches: Vec<SequencesSketch>) -> SequencesSketch{
    assert!(!sketches.is_empty());
    let re = Regex::new(r"(.+)(1|2)(\..+)").unwrap();
    let num_sketches = sketches.len();
    let mut first_sketch = std::mem::take(&mut sketches[0]);
    for i in 1..num_sketches{
        let x = std::mem::take(&mut sketches[i]);
        for (key,val) in x.kmer_counts.into_iter(){
            let count = first_sketch.kmer_counts.entry(key).or_insert(0);
            *count += val;
        }
    }
    for cap in re.captures_iter(&first_sketch.file_name.clone()) {
        let new_file_name = format!("{}{}",&cap[1],&cap[3]);
        first_sketch.file_name = new_file_name;
    }
    return first_sketch;
}

//Collect paired-end reads names together.
fn collect_pairs_file_names<'a> (args: &'a SketchArgs) -> Vec<Vec<&str>>{

    let mut list_pair1 : HashMap<_,_>= HashMap::default();
    let mut ret_pairs = vec![];
    let re = Regex::new(r"(.+)(1|2)(\..+)").unwrap();
    if args.sequences.is_some(){
        for read_file in args.sequences.as_ref().unwrap().iter(){
            if re.is_match(read_file){
                let front = re.captures(read_file).unwrap().get(1).unwrap().as_str();
                let pair = list_pair1.entry(front).or_insert(vec![]);
                pair.push(read_file.as_str());
            }
            else{
                ret_pairs.push(vec![read_file.as_str()]);
            }
        }
    }
    for (_,files) in list_pair1{
        if files.len() == 1 || files.len() == 2{
            ret_pairs.push(files);
        }
        else if files.len() > 2{
            log::warn!("Something went wrong with paired-end read processing. Treating pairs in {:?} as separate.", &files);
            for file in files{
                ret_pairs.push(vec![file]);
            }
        }
    }

    return ret_pairs;

}

pub fn sketch(args: SketchArgs) {
    let sequences = &args.sequences;
    if sequences.is_some(){
        info!("Sketching sequences...");
        let paired_sequences = collect_pairs_file_names(&args);
        trace!("Paired sequences are: {:?}", &paired_sequences);
        let iter_vec : Vec<usize> = (0..paired_sequences.len()).into_iter().collect();
        iter_vec.into_par_iter().for_each(|i| {
            let mut sequence_sketches = vec![];
            let sequence_files = &paired_sequences[i];
            let pref = Path::new(&args.sequence_output_prefix);

            for sequence_file in sequence_files{
                let sequence_sketch = sketch_query(&args, sequence_file);
                sequence_sketches.push(sequence_sketch);
            }

            let combined_sketch = combine_sketches(sequence_sketches);

            let sequences_file_path = Path::new(&combined_sketch.file_name).file_name().unwrap();
            let file_path = pref.join(&sequences_file_path);
            let file_path_str = format!("{}.prs", file_path.to_str().unwrap());

            let mut query_sk_file = BufWriter::new(File::create(&file_path_str).expect(&format!("{} not valid", file_path_str)));

            let enc = SequencesSketchEncode::new(combined_sketch);
            bincode::serialize_into(&mut query_sk_file, &enc).unwrap();
            info!("Sketching {} complete.", file_path_str);
        });
    }

    let genomes = &args.genomes;
    if genomes.is_some(){
        let genomes = genomes.as_ref().unwrap();
        info!("Sketching genomes...");
        let iter_vec : Vec<usize> = (0..genomes.len()).into_iter().collect();
        iter_vec.into_par_iter().for_each(|i| {
            let genome_file = &genomes[i];
            let pref = Path::new(&args.genome_output_prefix);
            let genome_file_name = Path::new(genome_file).file_name().unwrap();
            let file_path = pref.join(genome_file_name);
            let file_path_str = format!("{}.prg", file_path.to_str().unwrap());
            let mut genome_sk_file = BufWriter::new(File::create(&file_path_str).expect(&format!("{} not valid", file_path_str)));
            let genome_sketch = sketch_genome(args.c, args.k, genome_file);
            if genome_sketch.is_some(){
                bincode::serialize_into(&mut genome_sk_file, &genome_sketch.unwrap()).unwrap();
            }
        });
    }
    
    info!("Finished.");
}

pub fn sketch_genome(c: usize, k: usize, ref_file: &str) -> Option<GenomeSketch>{
    let reader = parse_fastx_file(&ref_file);
    let mut vec = vec![];
    if !reader.is_ok() {
        warn!("{} is not a valid fasta/fastq file; skipping.", ref_file);
        return None
    } else {
        let mut reader = reader.unwrap();
        let mut first = true;
        let mut return_genome_sketch = GenomeSketch::default();
        return_genome_sketch.c = c;
        return_genome_sketch.k = k;
        return_genome_sketch.file_name = ref_file.to_string();
        while let Some(record) = reader.next() {
            if record.is_ok() {
                let record = record.expect(&format!("Invalid record for file {}", ref_file));
                if first {
                    let contig_name = String::from_utf8_lossy(record.id()).to_string();
                    return_genome_sketch.first_contig_name = contig_name;
                    first = false;
                }
                let seq = record.seq();
                unsafe {
                    extract_markers_avx2(&seq, &mut vec, c, k);
                }
            } else {
                warn!("File {} is not a valid fasta/fastq file", ref_file);
            }
        }
        let mut kmer_set = MMHashSet::default();
        let mut duplicate_set = MMHashSet::default();
        let mut new_vec = Vec::with_capacity(vec.len());
        for km in vec.iter() {
            if !kmer_set.contains(&km) {
                kmer_set.insert(km);
            }
            else{
                duplicate_set.insert(km);
            }
        }
        for km in vec.iter(){
            if !duplicate_set.contains(&km){
                new_vec.push(*km);
            }
        }
        return_genome_sketch.genome_kmers = new_vec;
        return Some(return_genome_sketch);
    }
}

pub fn sketch_query(args: &SketchArgs, query_file: &str) -> SequencesSketch {
    let read_file = query_file;
    let mut read_sketch = SequencesSketch::new(read_file.to_string(), args.c, args.k);
    if read_file.contains(".fq") || read_file.contains(".fastq") {
        let reader;
        if read_file.contains(".gz") || read_file.contains(".gzip") {
            let file = File::open(read_file).unwrap();
            let gz_decoder: Box<dyn Read + Send> = Box::new(BufReader::new(GzDecoder::new(file)));
            reader = ReaderQ::new(gz_decoder);
        } else {
            let file = File::open(read_file).unwrap();
            let decoder: Box<dyn Read + Send> = Box::new(BufReader::new(file));
            reader = ReaderQ::new(decoder);
        }
        read_parallel(
            reader,
            args.threads as u32,
            100,
            |record_set| {
                let mut vec = vec![];
                unsafe {
                    for record in record_set.into_iter() {
                        //                        dbg!(String::from_utf8(record.seq().to_vec()));
                        extract_markers_avx2(record.seq(), &mut vec, args.c, args.k);
                    }
                }

                return vec;
            },
            |record_sets| {
                while let Some(result) = record_sets.next() {
                    let (_rec_set, vec) = result.unwrap();
                    for km in vec {
                        let c = read_sketch.kmer_counts.entry(km).or_insert(0);
                        *c += 1;
                    }
                }
            },
        );
    } else {
        let reader;
        if read_file.contains(".gz") || read_file.contains(".gzip") {
            let file = File::open(read_file).unwrap();
            let gz_decoder: Box<dyn Read + Send> = Box::new(BufReader::new(GzDecoder::new(file)));
            reader = ReaderA::new(gz_decoder);
        } else {
            let file = File::open(read_file).unwrap();
            let decoder: Box<dyn Read + Send> = Box::new(BufReader::new(file));
            reader = ReaderA::new(decoder);
        }
        read_parallel(
            reader,
            args.threads as u32,
            100,
            |record_set| {
                // this function does the heavy work
                let mut vec = vec![];
                unsafe {
                    for record in record_set.into_iter() {
                        extract_markers_avx2(record.seq(), &mut vec, args.c, args.k);
                    }
                }

                return vec;
            },
            |record_sets| {
                while let Some(result) = record_sets.next() {
                    let (_rec_set, vec) = result.as_ref().unwrap();
                    for km in vec {
                        let c = read_sketch.kmer_counts.entry(*km).or_insert(0);
                        *c += 1;
                    }
                }
            },
        );
    }

    return read_sketch;
}


