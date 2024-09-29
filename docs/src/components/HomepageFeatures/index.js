import clsx from 'clsx';
import Heading from '@theme/Heading';
import styles from './styles.module.css';

const images = [
  {
    title: 'Comprehensive Dashboard',
    src: require('@site/static/img/screenshot1.png').default,
    description: (
      <>
        AURCache provides a comprehensive dashboard that gives you a great overview of your repository status. You can easily monitor package versions, build statuses, and repository health at a glance, ensuring you stay informed about the state of your packages.
      </>
    ),
  },
  {
    title: 'Detailed Build Logs',
    src: require('@site/static/img/screenshot2.png').default,
    description: (
      <>
        The build output page in AURCache provides detailed logs for each package build. You can easily track build progress, view logs, and diagnose issues. This feature helps you maintain control over the build process and quickly address any problems that arise.
      </>
    ),
  },
  {
    title: 'User-Friendly Interface',
    src: require('@site/static/img/screenshot3.png').default,
    description: (
      <>
        With its clear and concise Flutter frontend, AURCache is designed to be easy to use. 
        Adding and Managing packages is straightforward, letting you focus on development without complex processes.
        Under the hood, the robust Rust backend guarantees stability and performance, providing a reliable foundation for all your repository needs.
      </>
    ),
  },
];

function AlternatingImages() {
  return (
    <div className={styles.container}>
      {images.map((image, index) => (
        <div key={index} className={clsx(styles.row, index % 2 === 0 ? styles.rowEven : styles.rowOdd)}>
            <>
              <div className={styles.text}>
                <Heading className="text--center padding-horiz--md" as="h3">{image.title}</Heading>
                <p className="text--center padding-horiz--md">{image.description}</p>
              </div>
              <div className={styles.image}><img src={image.src} /></div>
            </>
        </div>
      ))}
    </div>
  );
}

export default function HomepageFeatures() {
  return (
    <section className={styles.features}>
      <div className="container">
        <div className="row">
          <AlternatingImages/>
        </div>
      </div>
    </section>
  );
}
