import clsx from 'clsx';
import Heading from '@theme/Heading';
import ThemedImage from '@theme/ThemedImage';
import styles from './styles.module.css';

const images = [
  {
    title: 'Comprehensive Dashboard',
    src: {
      light: require('@site/static/img/screenshot1.png').default,
      dark: require('@site/static/img/screenshot1-dark.png').default,
    },
    description: (
      <>
        AURCache provides a comprehensive dashboard that gives you a great overview of your repository status. You can easily monitor package versions, build statuses, and repository health at a glance, ensuring you stay informed about the state of your packages.
      </>
    ),
  },
  {
    title: 'Package Management',
    src: {
      light: require('@site/static/img/screenshot2.png').default,
      dark: require('@site/static/img/screenshot2-dark.png').default,
    },
    description: (
      <>
        Each package gets its own page with its builds, dependencies and dependents, produced artifacts, and build configuration. Rebuilding, changing platforms and flags, editing sources, or removing the package all happen in one place.
      </>
    ),
  },
  {
    title: 'Detailed Build Logs',
    src: {
      light: require('@site/static/img/screenshot3.png').default,
      dark: require('@site/static/img/screenshot3-dark.png').default,
    },
    description: (
      <>
        The build output page in AURCache provides detailed logs for each package build. You can easily track build progress, view logs, and diagnose issues. This feature helps you maintain control over the build process and quickly address any problems that arise.
      </>
    ),
  },
  {
    title: 'Responsive Mobile View',
    src: {
      light: require('@site/static/img/screenshot_mobile1.png').default,
      dark: require('@site/static/img/screenshot_mobile1-dark.png').default,
    },
    description: (
      <>
        The interface adapts to narrow screens, so you can monitor builds, check package status, and read logs from your phone just as easily as from your desktop.
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
              <div className={styles.image}><ThemedImage sources={image.src} alt={image.title} /></div>
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
